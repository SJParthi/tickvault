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

- [ ] **Item 4** — Gap-fill writes target `historical_candles` table (LOCKED 2026-05-17)
  - **DECISION (locked PR-B session, see `audit-findings-2026-04-17.md` Rule 15):**
    Gap-fill REST-fetched 1m bars write to the EXISTING `historical_candles`
    table via the EXISTING `candle_persistence.rs` writer. NO new shadow
    column, NO new table, NO new helper fn — reuse `CandlePersistenceWriter`
    as-is.
  - **Why historical_candles:**
    - Matches `live-feed-purity.md` rule: "Historical data lives in two
      places only: `historical_candles` table — Dhan REST /charts/historical
      results". Gap-fill IS REST-fetched historical data.
    - DEDUP key `(ts, security_id, timeframe, segment)` already segment-aware
      per I-P1-11 (see `crates/storage/src/candle_persistence.rs::DEDUP_KEY_CANDLES`).
    - `ensure_candle_table_dedup_keys()` already runs at boot — no schema
      migration needed.
    - `cross_verify.rs` already READS this table — gap-fill rows flow into
      the existing read path naturally.
  - **`source` SYMBOL column NOT added in this series** — gap-fill rows
    are distinguishable from boot-time backfill by `(ts, timeframe='1m')`
    inside the 1-3 minute window after a disconnect event. If forensic
    distinction is later needed, add `source` SYMBOL via
    `ALTER ADD COLUMN IF NOT EXISTS` in a follow-up PR.
  - PR-C uses `CandlePersistenceWriter::write_candle()` directly; no new
    storage-crate helper needed.

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

## Locked architectural decisions (PR-B' session, 2026-05-17)

These decisions resolve the deferrals from PR-A so PR-C opens with zero
TBDs (per `audit-findings-2026-04-17.md` Rule 17):

| Question | Locked decision | Reason / source |
|---|---|---|
| Gap-fill write target table | `historical_candles` (existing) | `live-feed-purity.md` rule: REST data → `historical_candles` |
| Write helper | EXISTING `CandlePersistenceWriter::write_candle()` | No new storage-crate fn needed; existing DEDUP key is segment-aware |
| `source` SYMBOL column | NOT added in this PR series | `(ts, timeframe='1m')` window in 1-3 min post-disconnect is sufficient identifier; ALTER ADD later if forensic distinction needed |
| Shutdown signal type | `Arc<Notify>` (existing codebase convention) | `instance_lock.rs` is the canonical reference; `tokio_util::sync::CancellationToken` is BANNED without operator approval per Rule 16 |
| `Notify::notify_waiters()` timing | Tasks MUST be parked on `.notified()` BEFORE the wake fires | Rule 16 trap — wakes lost if signal arrives before `select!` parks |
| REST fetch fn visibility | `fetch_intraday_with_retry` is currently private; PR-C MUST refactor to a pub wrapper or accept a Sender-pattern handoff | See `crates/core/src/historical/candle_fetcher.rs:1276` |
| Connection-layer broadcast Sender | Threaded through `WebSocketConnection::new()` constructor; one canonical post-reconnect send site at `connection.rs:~580` (next to existing `ws_reconnect_audit` write) | Only ONE site emits; do NOT spread Sender::send() across multiple reconnect-handler arms |
| Pool size + channel capacity | `broadcast::channel(64)` — 5 conns × 2s flap × 25s drain budget | Documented; Lagged fires `GapFill04EventChannelLagged` Critical per audit-findings Rule 11 |
| Heartbeat counter | NOT shipped in PR-B'. Only ship when an ACTUAL work-loop exists for it to claim alive about | Rule 14 — false-OK heartbeat anti-pattern |
| Skeleton PR pattern | BANNED (Rule 14). PR-C ships as a vertical slice with REAL call sites + REAL work, OR ships in a fresh focused session — no fragmenting | Two CRITICAL + four HIGH findings from PR-B 3-agent review |

## PR-A Status (2026-05-17)

**Items 1, 2, 3 + security fixes** complete and committed:
- ✅ Item 1 (constants) — commit `5f93f1a`
- ✅ Item 2 (ErrorCode + rule file) — commit `5f93f1a`
- ✅ Item 3 (Telegram event variants) — commit `fee4f60`
- ✅ Security agent MEDIUM #1 + #2 (DDL `error!` + body cap) — commit `fee4f60`
- 🔄 Item 4 (UPSERT helper) — DEFERRED to PR-B
- 🔄 Items 5-11 — PR-B + PR-C

**Why split:** Item 4 needs target-table decision (`candles_1m` is currently a matview, Phase 0 native-base migration in flight). Items 1-3 + security fixes are pure additive — zero behavior change, low review risk.

## PR-B Status (2026-05-17) — ABORTED

PR-B was attempted twice in the 2026-05-17 session:

1. **Original PR-B (Items 4-11 all-at-once)** — abandoned as too large
   for one session (1500-2500 LoC across `connection.rs` + `main.rs` +
   new scheduler module + config + tests + Grafana panel).
2. **Re-scoped PR-B (scheduler skeleton only)** — built on branch
   `claude/phase-0-gap-fill-scheduler-skeleton`, ~450 LoC. Aborted by
   3-agent adversarial review with **2 CRITICAL + 4 HIGH** findings
   BEFORE any commit. Branch never pushed. See `audit-findings-2026-04-17.md`
   Rule 14 (Skeleton-PR anti-pattern) for the full lesson.

PR-B' (this PR) captures the architectural decisions that resolve all
PR-A deferrals so PR-C opens with zero TBDs. PR-C ships the full
vertical slice — REAL Sender::send() wired in `connection.rs`, REAL
scheduler that consumes events + calls planner + fetches via existing
candle_fetcher + UPSERTs to existing `historical_candles` table + writes
audit row, REAL boot wiring, REAL integration tests — in a fresh
focused session.

## PR-B' Status (2026-05-17) — THIS PR (docs only)

- ✅ `audit-findings-2026-04-17.md` Rules 14, 15, 16, 17 added
- ✅ Plan-file "Locked architectural decisions" table added
- ✅ Plan-file PR-B abort logged with reference to Rule 14
- ❌ No production code changes — docs only

## PR-C Status (2026-05-17) — MERGED (#671)

PR-C shipped the vertical-slice broadcast plumbing: `DisconnectResolvedEvent`
type + `WebSocketConnection::with_disconnect_event_sender()` builder + canonical
post-reconnect send-site + `run_gap_fill_scheduler` task with planner + 3 Prom
counters + boot wiring in `main.rs`. 3-agent review NO CRITICAL/NO HIGH. 11
tests pass.

## PR-D1 Status (2026-05-17) — THIS PR (visibility promotion)

Minimal visibility-promotion PR to unblock PR-D2 (the actual REST fetch + UPSERT).
Promotes 6 private items in `candle_fetcher.rs` to `pub(crate)` so PR-D2's
per-bar fetch helper can reuse the existing retry/backoff/error-classification
logic without duplicating it.

Items promoted:

| Item | From | To | Why PR-D2 needs it |
|---|---|---|---|
| `ErrorAction` enum | private | `pub(crate)` | Classify Dhan error response for retry decisions |
| `classify_error()` | private | `pub(crate)` | Run the classification logic |
| `compute_dh904_backoff_secs()` | private | `pub(crate)` | DH-904 ladder: 10s, 20s, 40s, 80s |
| `is_dh904_exhausted()` | private | `pub(crate)` | Decide when to give up |
| `compute_retry_delay_ms()` | private | `pub(crate)` | 5xx linear backoff |
| `IntradayRequest` struct | private | `pub(crate)` | Construct Dhan REST request body |

Plus: every field of `IntradayRequest` is now `pub` (was implicit `private`)
so the scheduler module can construct it.

ZERO behavioural change — only visibility keywords. 220 existing
`candle_fetcher` tests pass without modification. No new pub fns (Rule 14
compliant). No new call sites needed in this PR — call sites land in PR-D2.

## PR-D2 Status (2026-05-17) — THIS PR (REST fetch pub helper)

Ships the new `pub async fn fetch_gap_fill_intraday_window` in
`candle_fetcher.rs` plus 3 pure-function helpers (`format_dhan_ist_datetime`,
`dhan_segment_string_from_code`, `build_gap_fill_intraday_request`) and
the typed `GapFillFetchError` enum.

The async fetch fn reuses the pub(crate) primitives promoted in PR-D1
(#672) — `classify_error`, `compute_dh904_backoff_secs`, `is_dh904_exhausted`,
`compute_retry_delay_ms`, `IntradayRequest` — so gap-fill follows the
SAME DH-904 ladder + 5xx retry + error classification as the boot-time
historical fetch.

NO writer coupling — caller UPSERTs the returned `Vec<HistoricalCandle>`
via the existing `CandlePersistenceWriter::write_candle()` per the
Item 4 architectural lock (target table: `historical_candles`).

NO scheduler wiring in this PR — that lands in PR-D3.

**Tests added (12):**
- 4 datetime formatter tests (epoch zero, one minute, full day, typical trading minute)
- 3 segment string tests (8 known + gap-at-6 + 9-255 unknown)
- 3 request builder tests (NIFTY IDX_I, gap-at-6 short-circuit, OI propagation)
- 2 error enum tests (Display includes diagnostic context, Send+Sync+'static)

Total candle_fetcher tests: 232 (was 220) — all passing.

## PR-D2 status archived (visibility promotion) — PR-D1 (#672 merged)

PR-D1 promoted 6 primitives in `candle_fetcher.rs` to `pub(crate)`.
ZERO behavioural change. 220 existing tests pass without modification.

## PR-D3 Status — LOCKED design (2026-05-17) — QUEUED for fresh session

This PR-D3 design is **locked** after PR-D2 (#673) merged the
`fetch_gap_fill_intraday_window` pub fn. PR-D3 wires that fn into the
production scheduler. Fresh focused session implements it per the
contract below — zero TBDs.

### What PR-D3 implements

1. **NEW `GapFillExecutorDeps` struct** in `gap_fill_scheduler.rs`:
   ```rust
   pub struct GapFillExecutorDeps {
       pub http_client: Arc<reqwest::Client>,
       pub endpoint: String,           // Dhan intraday endpoint URL
       pub token_handle: TokenHandle,  // Arc<ArcSwap<Option<TokenState>>>
       pub client_id: SecretString,
       pub candle_writer: Arc<tokio::sync::Mutex<CandlePersistenceWriter>>,
       pub max_retries: u32,
   }
   ```

2. **RENAME `run_gap_fill_scheduler` → `run_gap_fill_executor`** —
   signature gains `deps: GapFillExecutorDeps` parameter. The 8 PR-C
   lifecycle tests are updated to construct deps with
   `reqwest::Client::new()` + a fresh `CandlePersistenceWriter` pointed
   at the test QuestDB instance (or `cfg(test)` no-op writer).

3. **Per-event executor logic** inside the renamed fn:
   ```rust
   // On receive event:
   let bars = compute_planned_bars(&event);
   for bar_start_secs in bars {
       let bar_end_secs = bar_start_secs + GAP_FILL_ONE_MINUTE_SECS as i64;
       // Wait bar_end + 10s seal buffer (BOOT-03 envelope)
       tokio::time::sleep_until(...).await;
       // Fetch via PR-D2 pub fn
       match fetch_gap_fill_intraday_window(
           &deps.http_client,
           &deps.endpoint,
           &load_access_token(&deps.token_handle)?,
           &deps.client_id,
           NIFTY_SECURITY_ID, // PR-D3a: hardcoded; PR-D3b iterates affected pool conn's instruments
           SEGMENT_CODE_IDX_I,
           "INDEX",
           bar_start_secs,
           bar_end_secs,
           false,
           deps.max_retries,
       ).await {
           Ok(candles) => {
               let mut writer = deps.candle_writer.lock().await;
               for c in &candles {
                   writer.append_candle(c)?;
               }
               writer.flush_if_needed()?;
               // Audit row + Telegram GapFillCompleted
           }
           Err(GapFillFetchError::TooManyConnections) => {
               tokio::time::sleep(Duration::from_secs(60)).await;
           }
           Err(GapFillFetchError::TokenExpired) => {
               // Token refresh handled elsewhere; skip + retry next event
           }
           Err(e) => {
               // Telegram GapFillFailed { error, attempt }
           }
       }
   }
   ```

4. **Boot wiring in `main.rs`**:
   - The existing `http_client`, `token_handle`, `dhan_config`,
     `client_id` handles are already constructed during boot for
     `fetch_historical_candles`.
   - PR-D3 creates a **dedicated** `CandlePersistenceWriter` for
     gap-fill (NOT shared with the boot-time historical fetcher to
     avoid lifetime conflicts). Wrapped in `Arc<tokio::sync::Mutex>`
     for future per-bar concurrent task spawning.
   - Build `GapFillExecutorDeps` and pass to `run_gap_fill_executor`
     via the existing spawn block.

5. **Telegram + audit wiring**:
   - On success per bar: write `gap_fill_audit` row with
     `result="success"`, `attempt=N`. Emit `GapFillCompleted` Info.
   - On retry-exhausted per bar: write `result="failed"`,
     `attempt=max`. Emit `GapFillFailed` Critical with the
     `GapFillFetchError` Display text.
   - Existing `append_gap_fill_audit_row` writer from PR-A is the
     persistence helper.

### What PR-D3 does NOT include (deferred to PR-D4)

- Per-instrument iteration. PR-D3 hardcodes NIFTY (security_id=13,
  IDX_I) as the only instrument fetched. This is acceptable for an
  MVP slice because:
  1. NIFTY is the most important index for trading decisions.
  2. The full call chain WS reconnect → broadcast → scheduler →
     fetch → UPSERT → audit is exercised end-to-end with one
     instrument, validating every wiring layer.
  3. PR-D4 generalises to iterate the affected connection's
     `instruments` list (requires `WebSocketConnection::instruments`
     getter — a 5-LoC change).
- Grafana panel + alert (PR-D4 or PR-D5).
- Rate limiting across N concurrent fetches (PR-D4 — `tokio::sync::Semaphore(GAP_FILL_MAX_CONCURRENT_FETCHES)`).
- `outage_start_secs` refinement using last-seen-tick timestamps
  (currently uses `now - 10s` placeholder per PR-C send-site).

### Estimated scope

~500-700 LoC across:
- `crates/core/src/historical/gap_fill_scheduler.rs` (~250 LoC)
- `crates/app/src/main.rs` (~50 LoC boot wiring)
- 8 PR-C lifecycle tests updated (~50 LoC)
- 5 new integration tests (~150 LoC)
- Plan file update

### Adversarial review checklist (run BEFORE commit per charter §E)

- [ ] hot-path-reviewer — verify the per-bar loop does NOT add hot-path allocations (cold path, ~5 bars/disconnect, ~5 disconnects/day)
- [ ] security-reviewer — verify access_token + client_id never appear in tracing labels or error messages
- [ ] general-purpose (hostile) — verify the writer Mutex doesn't deadlock with the boot-time historical writer; verify the dedicated writer's rescue-ring is separate from the live writer's

### Why this design respects all 17 audit-findings rules

- Rule 14 (no skeleton): every pub fn has a real call site (boot wires `run_gap_fill_executor`)
- Rule 15 (locked decisions): every decision above is fixed in this plan file
- Rule 16 (Arc<Notify> shutdown): inherited from PR-C; not changed
- Rule 17 (front-loaded investigation): PR-D1 + PR-D2 + this plan section did the investigation; PR-D3 is pure implementation

## PR-D4 Status — SHIPPED 2026-05-17 (indices fan-out + audit + Telegram)

Branch: `claude/phase-0-gap-fill-pr-d4-indices-fanout`.

### What PR-D4 shipped

1. **Indices-only fan-out** — replaced PR-D3's hardcoded NIFTY with
   `GAP_FILL_INDICES: &[(u32, u8, &str); 4]` covering NIFTY=13,
   BANKNIFTY=25, SENSEX=51, INDIA VIX=21. Scope locked per
   `websocket-connection-scope-lock.md` §I.
2. **`execute_gap_fill_for_bar` generalised** — accepts
   `(security_id, segment_code, instrument_type)` parameters.
3. **New per-bar driver `execute_bar_for_all_indices`** — iterates
   the 4 SIDs, tallies `(sids_completed, sids_failed)`, emits one
   audit row + one Telegram per bar (NOT per SID — operator wants
   bar-scoped, not noise).
4. **`gap_fill_audit` row writes** — `(trading_date_ist, bar_minute,
   sids_requested=4, sids_completed, sids_failed, duration_ms,
   result ∈ {success,partial,failed})` per bar. Audit-row write
   failure logs `GAP-FILL-03` but does NOT fail the bar (candles
   already UPSERTed).
5. **Telegram emission** — `GapFillCompleted` Info on full success,
   `GapFillPartial` High with sample-of-5 failed SIDs on mixed
   outcome, `GapFillFailed` Critical with last error + attempt count
   when zero SIDs succeed.
6. **`GapFillExecutorDeps` extended** with `questdb_config` +
   `notification_service`. Boot wiring in `main.rs` passes
   `config.questdb.clone()` + `Arc::clone(&notifier)`.
7. **New metric** — `tv_gap_fill_bars_partial_total`. Semantics
   tightened: `bars_succeeded` now = ALL 4 SIDs ok per bar (was: per
   bar regardless of NIFTY-only outcome under PR-D3).

### What PR-D4 does NOT include (deferred to PR-D5)

- NSE_EQ equities fan-out (~218 SIDs) — needs
  `tokio::sync::Semaphore(GAP_FILL_MAX_CONCURRENT_FETCHES)` +
  per-conn instrument snapshot on `DisconnectResolvedEvent`.
- `outage_start_secs` refinement (still uses PR-C's
  `now - estimated_outage_secs` placeholder).
- `GapFill04EventChannelLagged` reconciliation pass (logged today;
  scheduler-catchup audit-row emission deferred).
- Grafana panel + alert rule for the 4 gap-fill counters (PR-D5/D6).

### Ratchet tests added (6 new)

- `test_gap_fill_indices_has_four_entries` — pins 4-SID scope
- `test_gap_fill_indices_security_ids_match_dhan_master` — pins SIDs
- `test_gap_fill_indices_all_idx_i_segment` — pins IDX_I segment
- `test_trading_date_ist_string_formats_yyyy_mm_dd` — date helper
- `test_bar_minute_ist_string_formats_hh_mm` — minute helper
- `test_telegram_failed_sid_sample_cap_matches_variant_doc` — cap

Replaced: `test_mvp_nifty_constants_pinned` (PR-D3 constant retired).

Total: 16/16 scheduler tests + 9/9 audit-persistence tests green.

## PR-D5 Status — SHIPPED 2026-05-17 (observability + silent-DDL fix)

Branch: `claude/phase-0-gap-fill-pr-d5-observability`.

### What PR-D5 shipped

1. **`ensure_gap_fill_audit_table` wired at boot** — added to the DDL
   `tokio::join!` in `main.rs` alongside the other audit-table helpers.
   This fixes a silent bug introduced by PR-A: the helper was defined +
   unit-tested but never called from production, so PR-D4's per-bar
   `append_gap_fill_audit_row` writes would fail "table not found"
   until something else happened to create the table. Rule 13 violation
   fixed.
2. **`tv_gap_fill_rest_latency_ms` histogram** — per-attempt wall-clock
   recorded in `fetch_gap_fill_intraday_window` (covers both happy-path
   and error-path attempts; surfaces Dhan REST tail latency).
3. **`tv_gap_fill_dh904_retries_total{outcome}`** — counter incremented
   in the DH-904 RateLimited branch, split by `outcome=retry|exhausted`.
4. **2 new Grafana panels** in operator-health.json (`y=100`):
   - "Gap-fill bars (5m, by outcome)" — 4 series wrapped in `increase()`
     per Rule 12 (succeeded/partial/failed/candles written).
   - "Gap-fill REST latency p99 / DH-904 retries (5m)" —
     `histogram_quantile(0.99, ...)` + DH-904 outcome breakdown.
5. **`GapFillBarsFailedHigh` Prometheus alert** — Critical, fires when
   `increase(tv_gap_fill_bars_failed_total[5m]) > 0` for ≥ 1 min.
   Pinned in `resilience_sla_alert_guard::REQUIRED_ALERT_NAMES`.
6. **`tv_gap_fill_` prefix added** to
   `grafana_dashboard_snapshot_filter_guard::REQUIRED_DASHBOARD_METRIC_PREFIXES`
   — future deletion of all gap-fill panels fails the build.
7. **`gap_fill_audit_boot_wiring_guard` ratchet test** — source-scans
   `main.rs` for `ensure_gap_fill_audit_table` to block Rule 13
   regression.

### What PR-D5 does NOT include (deferred to PR-D6)

- NSE_EQ equities fan-out (~218 SIDs) gated by
  `tokio::sync::Semaphore(GAP_FILL_MAX_CONCURRENT_FETCHES)`
- Per-conn instrument-snapshot extension to `DisconnectResolvedEvent`
  (Arc<Vec<InstrumentRef>> payload)
- `outage_start_secs` refinement using last-seen-tick timestamps
- Scheduler-catchup audit-row emission on `Lagged(n)` events
  (currently logged Critical only)

Each of these has architectural decisions that need a focused plan
session per Rules 15 + 17.

### Verification

- 1841/1841 `tickvault-storage` tests green
- 469/469 `tickvault-core` historical tests green
- `cargo fmt --check` clean
- `banned-pattern-scanner.sh` clean
- `pub-fn-test-guard.sh` clean (untested pub fn count stable at 143)
- `pub-fn-wiring-guard.sh` clean
- `grafana_dashboard_snapshot_filter_guard` 41/41 tests green
- `resilience_sla_alert_guard` 5/5 tests green
- `gap_fill_audit_boot_wiring_guard` 1/1 test green
- Dashboard JSON + alerts YAML both parse clean

## PR-D6 Status — THIS PR (NSE_EQ fan-out + Semaphore parallelism)

### Locked architectural decisions (Rule 15, 2026-05-17)

The operator delegated all 4 decisions. Locked as follows after engineering
review against Rules 14 (skeleton anti-pattern), 17 (front-load investigation),
and the hot-path / live-feed-purity rules:

| # | Decision | Rationale |
|---|---|---|
| 1 | **PR-D6 ships all-SIDs fan-out (no per-conn precision).** Every disconnect event triggers gap-fill across both 4 IDX_I + NSE_EQ snapshot. | Per-conn precision requires extending `DisconnectResolvedEvent` from `Copy 24 bytes` to a non-Copy struct carrying `Arc<Vec<u32>>` — that's a separate vertical slice. PR-D6 ships the meaty NSE_EQ slice without the event-payload refactor. Cost: 218 extra REST calls per disconnect (vs ~50 typical per-conn). At ~5 disconnects/day expected, that's ~1100 calls/day — well under Dhan's 100K/day Data API budget. |
| 2 | **Semaphore capacity = `GAP_FILL_MAX_CONCURRENT_FETCHES = 5`.** | Matches Dhan Data API hard limit of 5/sec. Constant already exists from PR-A. |
| 3 | **Within-bar parallelism via `tokio::task::JoinSet` + Semaphore permits.** Each per-SID fetch spawned as task, gated by `acquire()`. | Sequential 222-SID loop = 44 sec per bar. Parallel cap 5 = ~9 sec per bar. Bar throughput stays under the broadcast 64-buffer 25s drain window even under flap-storm. |
| 4 | **Defer per-conn precision (event payload extension) to PR-D7.** | Vertical slice — requires `DisconnectResolvedEvent` refactor across producer + consumer + 5 test sites + boot wiring. |
| 5 | **Defer `outage_start_secs` refinement (TickGapTracker wiring) to PR-D8.** | Requires plumbing `Arc<TickGapTracker>` reference into `WebSocketConnection` — separate refactor. |
| 6 | **Defer `GapFill04EventChannelLagged` reconciliation pass to PR-D9.** | Audit-row-only without reconciliation = Rule 14 false-OK anti-pattern. Full reconciliation needs SELECT against `historical_candles`, gap-detection logic, synthetic plan dispatch — its own vertical slice. |

### What PR-D6 ships

- `GapFillExecutorDeps`: 2 new fields — `nse_eq_sids: Arc<Vec<u32>>` (snapshot from `InstrumentRegistry.by_exchange_segment()[NseEquity]` at boot), `fetch_semaphore: Arc<Semaphore>` (capacity = `GAP_FILL_MAX_CONCURRENT_FETCHES`).
- `execute_bar_for_all_indices` renamed to `execute_bar_for_all_instruments`; iterates `GAP_FILL_INDICES` + `nse_eq_sids` via `JoinSet`; each task acquires a permit before fetching.
- Boot wiring in `main.rs` Step 8.x: snapshot NSE_EQ sec_ids from registry before constructing `GapFillExecutorDeps`. Logged via `info!` with count + bytes for forensics.
- Audit row's `sids_requested` now reflects the bigger count (4 + |nse_eq_sids|).
- 3 ratchet tests:
  - `test_gap_fill_executor_deps_carries_nse_eq_sids_and_semaphore` — struct shape pinning
  - `test_gap_fill_executor_deps_new_accepts_nse_eq_snapshot` — constructor wiring
  - `test_semaphore_capacity_equals_gap_fill_max_concurrent_fetches_constant` — capacity invariant

### What PR-D6 does NOT include (deferred to PR-D7/D8/D9)

- Per-conn precision via `DisconnectResolvedEvent.sec_ids: Arc<Vec<u32>>` (PR-D7)
- `outage_start_secs` refinement using last-seen-tick from `TickGapTracker` (PR-D8)
- `GapFill04EventChannelLagged` reconciliation SELECT pass (PR-D9)

## PR-D7 Status — THIS PR (per-conn precision via event payload extension)

### What PR-D7 ships

Extends `DisconnectResolvedEvent` from a `Copy` 24-byte struct to a
`Clone + Eq` struct carrying `subscribed: Arc<Vec<(u32, u8)>>` —
the per-conn snapshot of `(security_id, segment_binary_code)` tuples
that the WebSocket producer captured at the moment of the broadcast.
The gap-fill scheduler now fans out ONLY across the SIDs the
affected connection had subscribed, instead of the global all-SIDs
fan-out shipped in PR-D6.

| Item | Detail |
|---|---|
| Event payload | `subscribed: Arc<Vec<(u32, u8)>>` (12 bytes per entry × up to 5,000 instruments per conn = ~60 KB Arc-shared) |
| Producer | `WebSocketConnection::new()` pre-computes the snapshot once from `instruments`; `disconnect_event_sender` send-site clones the Arc |
| Consumer | Segment filter accepts `0` (IDX_I) and `1` (NSE_EQ) only; `tv_gap_fill_skipped_segment_total` counter tracks dropped derivative SIDs |
| Empty-snapshot guard | `event.subscribed.is_empty()` → log info + increment `tv_gap_fill_empty_snapshot_total` + skip bar loop (no audit, no Telegram) |
| Boot wiring cleanup | Removed PR-D6's `gap_fill_nse_eq_sids` registry snapshot — the per-conn payload supersedes it |
| Struct cleanup | Dropped `GapFillExecutorDeps.nse_eq_sids` field; removed dead `GAP_FILL_INDICES` hard-coded array |

### Ratchet tests added

- `test_event_arc_shares_snapshot_across_clones` — Arc strong-count assertion across clone/drop
- `test_event_is_clone_debug_eq` — struct shape pinning (post-Copy removal)
- `test_disconnect_event_carries_arc_subscribed_snapshot` — payload field pinning
- `test_per_conn_segment_filter_keeps_idx_i_and_nse_eq_only` — segment eligibility invariant
- `test_gap_fill_executor_deps_carries_semaphore` — Semaphore field still present (PR-D6 ratchet rebased)

### Verification

- 34/34 `tickvault-core` gap_fill module tests green
- 5/5 `disconnect_event` tests green
- 440/440 `websocket` module tests green
- `cargo fmt --check` clean
- `banned-pattern-scanner.sh` clean
- `pub-fn-test-guard.sh` clean (untested pub fn count stable at 143)
- `pub-fn-wiring-guard.sh` clean

### What PR-D7 does NOT include (deferred to PR-D8/D9)

- `outage_start_secs` refinement using last-seen-tick from `TickGapTracker` (PR-D8)
- `GapFill04EventChannelLagged` reconciliation SELECT pass (PR-D9)

## PR-D8 Status — THIS PR (outage_start refinement via shared last-seen LTT cache)

### What PR-D8 ships

Introduces `LastSeenLttCache` — a lock-free `Arc<papaya::HashMap<(u32, u8), i64>>` keyed by `(security_id, segment_code)` per I-P1-11. tick_processor writes the last-seen `exchange_timestamp` for every parsed tick; the gap-fill scheduler reads at disconnect-event handling time to refine `event.outage_start_secs` from the producer's conservative `now - 10s` to the OLDEST last-seen LTT across the conn's subscribed SIDs.

| Item | Detail |
|---|---|
| New module | `crates/core/src/pipeline/last_seen_ltt_cache.rs` — `LastSeenLttCache` type mirrors `PrevOiCache` pattern |
| Hot-path write | tick_processor sites in `run_tick_persistence_consumer` + `run_slow_boot_observability` call `cache.record(sid, segment, ltt)` per tick |
| Cold-path read | `gap_fill_scheduler` calls `deps.last_seen_ltt_cache.refine_outage_start(event.outage_start_secs, event.subscribed)` before `compute_planned_bars` |
| Refinement semantics | `min(producer_estimate, min(last_seen for each subscribed sid))` — picks the OLDER bound so any gap > 10s is detected |
| New counter | `tv_gap_fill_outage_start_refined_total` increments when the refinement changes the estimate |
| Boot wiring | Slow boot constructs the cache once, clones into `run_slow_boot_observability` + `GapFillExecutorDeps`; fast boot passes a disposable empty cache |

### Architectural decisions (locked)

| # | Decision | Why |
|---|---|---|
| 1 | **Scheduler-side refinement, not producer-side** | Producer (`WebSocketConnection`) doesn't own the tick stream; routing every tick through the conn would double hot-path cost |
| 2 | **`Arc<papaya::HashMap<(u32, u8), i64>>` for storage** | Lock-free; mirrors existing `PrevOiCache` pattern in codebase |
| 3 | **`i64` storage, not `AtomicI64`** | LTT updates monotonically; race-on-insert produces at most a few seconds of skew, well within refinement tolerance |
| 4 | **Composite `(u32, u8)` key per I-P1-11** | `security_id` alone is NOT unique across segments |
| 5 | **`min` not `max` for refinement** | Conservative — the OLDER last-seen bounds the outage window; refilling from `T` ensures no gap is missed |

### Ratchet tests added (12)

- 10 in `last_seen_ltt_cache::tests` covering empty cache, record/get roundtrip, I-P1-11 segment distinction, overwrite, all 6 refinement paths, Arc-share semantics
- 2 in `gap_fill_scheduler::tests`: `test_gap_fill_executor_deps_carries_last_seen_ltt_cache`, `test_refine_outage_start_picks_older_cache_value_over_producer_estimate`

### Verification

- 12/12 `last_seen_ltt_cache` tests green
- 36/36 `gap_fill_scheduler` tests green
- `cargo check -p tickvault-core -p tickvault-app` green
- `cargo fmt --check` clean
- `banned-pattern-scanner.sh` clean
- `pub-fn-test-guard.sh` clean (untested pub fn count stable at 143)
- `pub-fn-wiring-guard.sh` clean

### What PR-D8 does NOT include (deferred to PR-D9)

- `GapFill04EventChannelLagged` reconciliation SELECT pass + audit row

## Original plan items mapped to PR series

| Plan Item | PR |
|---|---|
| Item 1 (constants) | PR-A (#668) ✅ |
| Item 2 (ErrorCodes) | PR-A (#668) ✅ |
| Item 3 (Telegram events) | PR-A (#668) ✅ |
| Item 4 (target table decision) | PR-B' (#670) ✅ locked |
| Item 5 (scheduler skeleton + supervisor) | PR-C (#671) ✅ |
| Item 5b (broadcast wiring) | PR-C (#671) ✅ |
| Item 6 (pub mod) | PR-C (#671) ✅ |
| Item 7 (boot wiring) | PR-C (#671) ✅ |
| Item 8 (Prom counters) | partial PR-C (3 counters); PR-D3 adds histogram |
| Item 9 (Grafana panel + alert) | PR-D3 |
| Item 10 (runbook) | PR-A (#668) ✅ |
| Item 11 (integration tests) | PR-C (#671) ✅ partial; PR-D2 adds REST tests |
| NEW: visibility promotion | PR-D1 (this PR) |
| NEW: REST fetch + UPSERT | PR-D2 |

## Verification before PR (PR-A)

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
