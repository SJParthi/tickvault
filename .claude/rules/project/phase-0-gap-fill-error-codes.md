# Phase 0 Gap-Fill Error Codes

> **Authority:** This file is the runbook target for the four ErrorCode
> variants added by Phase 0 Items 8+9 (gap-fill scheduler). Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires every
> variant in `crates/common/src/error_code.rs::ErrorCode` to be mentioned
> in at least one rule file under `.claude/rules/`. This file serves that
> contract for the gap-fill code family.
>
> **Cross-references:**
> - Plan: `.claude/plans/active-plan-item-8-9-gap-fill.md`
> - Phase 0 LOCKED: `.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` §3 + §7
> - Live-feed purity: `.claude/rules/project/live-feed-purity.md` (gap-fill writes only to `candles_1m`, never `ticks`)
> - Operator-charter: §H (one-PR-at-a-time), §I (WS connection scope lock)

## GAP-FILL-01 — scheduler supervisor caught panic / channel closed

**Trigger:** the gap-fill scheduler supervisor task (`spawn_gap_fill_scheduler`)
observed the inner per-disconnect handler returning unexpectedly — either:

1. A `JoinSet` per-bar task panicked and the supervisor caught the panic via
   `JoinHandle::await`'s `Err(JoinError::Panic(_))` branch.
2. The disconnect-event `broadcast::Receiver::recv()` returned
   `Err(RecvError::Closed)` because every `Sender` half was dropped (boot-time
   wiring bug — should never happen in a healthy app).

Severity::Critical. Bounded-zero-loss envelope is breached until the
supervisor re-spawns the inner task. The L3 safety net is the post-market
`cross_verify.rs` run at ~17:00 IST which compares historical vs live
candles and emits its own Critical Telegram on mismatch.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — look for the panic backtrace
   immediately preceding the GAP-FILL-01 event. Common causes: division
   by zero in retry-delay calculation, panic in the broadcast subscriber.
2. `tv_gap_fill_scheduler_heartbeat_total` — if the heartbeat counter
   resumed incrementing after the GAP-FILL-01 event, the supervisor
   successfully re-spawned and the scheduler is healthy again.
3. If the heartbeat is STILL stale (`increase(...[3m]) == 0` for 3+
   minutes), the supervisor itself died — restart the app.

**Auto-triage safe:** NO (Severity::Critical).

**Source (planned):** `crates/core/src/historical/gap_fill_scheduler.rs::spawn_gap_fill_scheduler`.

## GAP-FILL-02 — per-bar REST fetch failed after all retries

**Trigger:** a per-bar Dhan REST `/v2/charts/intraday` fetch failed after
exhausting either:

- `GAP_FILL_RETRY_ATTEMPTS = 3` generic-5xx retries with
  `GAP_FILL_RETRY_BACKOFF_SECS = [2, 5, 10]` backoff, OR
- 4 DH-904 (rate-limit) retries with `GAP_FILL_DH904_BACKOFF_SECS = [10, 20, 40, 80]` backoff (per `compute_dh904_backoff`).

Severity::High. The bar's audit row is written with `result=failed` and
the operator decides whether to backfill manually via the bhavcopy
end-of-day reconciliation.

**Triage:**
1. `mcp__tickvault-logs__questdb_sql "select * from gap_fill_audit where result = 'failed' and ts > now() - 1h"`
   — inspect the failed bars.
2. Cross-check Dhan API status — was there a 5xx outage during the
   failure window? If yes, this is expected; operator decides whether
   to manually backfill.
3. `tv_gap_fill_bars_completed_total{result="failed"}` rate — a
   sustained > 0.1/s rate (more than 1 failure per 10s) indicates a
   systemic Dhan-side issue or our rate-limit budget is being
   exceeded; check `Dh904RateLimit` counter and back off the scheduler
   concurrency cap (`GAP_FILL_MAX_CONCURRENT_FETCHES`).

**Auto-triage safe:** YES (High severity; visibility only — no destructive
action recommended).

**Source (planned):** `crates/core/src/historical/gap_fill_scheduler.rs`
inner per-bar retry loop.

## GAP-FILL-03 — QuestDB UPSERT failed after successful REST fetch

**Trigger:** a per-bar fetch succeeded (Dhan returned the bar OHLCV
payload) but the subsequent `candle_persistence::upsert_gap_fill_bar()`
call returned an error. Common causes: QuestDB ILP TCP port unreachable,
schema drift on `candles_1m` (a column was added without the boot-time
ALTER), or rescue-ring + spill + DLQ all saturated under chaos.

Severity::High. The audit row already reflects the REST success
(`result=success` with attempt N), but the QuestDB row was NOT written —
so the operator must reconcile manually OR rely on post-market
cross-verify.

**Triage:**
1. `mcp__tickvault-logs__docker_status` — is QuestDB OFFLINE?
2. `mcp__tickvault-logs__questdb_sql "select count(*) from candles_1m where ts > now() - 1h"`
   — is the table accepting writes at all?
3. `ls -la data/spill/` — is the spill directory filling?
4. If QuestDB is healthy and spill is empty, the failure was transient;
   the rescue-ring writer will retry the buffered row on its next flush.

**Auto-triage safe:** YES (High severity; QuestDB rescue tier handles
recovery if the underlying issue resolves within rescue-ring capacity).

**Source (planned):** `crates/storage/src/candle_persistence.rs::upsert_gap_fill_bar`
+ `crates/core/src/historical/gap_fill_scheduler.rs` inner per-bar task.

## GAP-FILL-04 — broadcast receiver lagged, disconnect events dropped

**Trigger:** the gap-fill scheduler's `tokio::sync::broadcast::Receiver::recv()`
returned `Err(RecvError::Lagged(n))` — meaning `n` disconnect-resolved
events were dropped silently before the scheduler could observe them.

Severity::Critical. Up to `n` outages may have been missed entirely by
the gap-fill scheduler — the corresponding 1m bars are at risk of being
silently absent from `candles_1m`. The scheduler responds by running a
full reconciliation pass: `SELECT count(*) FROM candles_1m WHERE ts
BETWEEN <window_start> AND <window_end>` for the suspected lagged
window, and triggers a synthetic gap-fill plan for any missing bars.

**Triage:**
1. `tv_gap_fill_event_channel_lagged_total{dropped}` — sum the counter
   to estimate cumulative event loss.
2. The reconciliation pass writes a `gap_fill_audit` row with
   `trigger_event = "scheduler_catchup"` per `topic-PHASE-0-LEAN-LOCKED.md`
   §3 schema — inspect those rows for the lagged window.
3. If the reconciliation itself failed (logged as a chained error),
   the operator must run the post-market cross-verify pass manually.

**Why it can happen:** broadcast channel capacity is bounded (default 64
in the planned wiring). A rapid burst of WS reconnects within < 1s
(e.g. infrastructure-level outage causing all 5 conns to flap
simultaneously) can saturate the buffer before the scheduler's `recv()`
loop drains it.

**Auto-triage safe:** NO (Severity::Critical; data-loss risk requires
operator confirmation that the reconciliation pass succeeded).

**Source (planned):** `crates/core/src/historical/gap_fill_scheduler.rs`
inner event-receive loop.
