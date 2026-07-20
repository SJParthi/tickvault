# Operator Daily Startup — 08:30 IST Pre-Market Checklist

> **Authority:** SEBI compliance + operator-charter §H.
> **Scope:** Every trading day. Operator MUST complete this before 09:15 IST market open.
> **Time-to-complete target:** 5 minutes if all green; 15 minutes if a recovery branch fires.
> **Companion:** `docs/runbooks/daily-operations.md` (behavioral discipline checklist).

## The 5 mandatory pre-market checks

| # | Check | Where | Pass criterion | If fail |
|---|---|---|---|---|
| 1 | Telegram `BootReadyConfirmation` arrived | Telegram chat | Single Severity::Info message dated today's IST date | Section A below |
| 2 | All 4 boot gates green | Same Telegram thread | `static_ip ✓ instance_lock ✓ clock_skew ✓ questdb_ready ✓` | Section B below |
| 3 | CloudWatch operator-health dashboard | CloudWatch console → Dashboards → operator-health | All 7 health widgets green; no firing alarms | Section C below |
| 4 | Main feed SIDs subscribed | CloudWatch "Main feed subscription count" widget | `tv_main_feed_subscribed_total` matches the expected universe size | Section D below |
| 5 | `previous_close` table populated for today | QuestDB | `SELECT count(*) FROM previous_close WHERE trading_date_ist = today()` returns ≥ 200 rows | Section E below |

## Time budget

```
08:25 — wake / coffee
08:30 — phone unlock + Telegram open
08:30 — check #1 BootReadyConfirmation
08:31 — check #2 4 boot gates
08:32 — open CloudWatch operator-health dashboard
08:33 — check #3 dashboard widgets
08:34 — check #4 subscription count
08:35 — open QuestDB console
08:36 — check #5 previous_close row count
08:37 — DONE (if all green) — Telegram "morning OK"
```

Anything beyond 08:50 IST without all 5 green = invoke fallback per
section letter and decide go/no-go by 09:00 IST.

## Section A — BootReadyConfirmation missing

**Most likely cause:** the app failed one or more boot gates and HALTed
without sending the Info message. Severity::Critical messages will be
present in the same Telegram thread.

  1. Check for `Severity::Critical` events in the last 12h.
  2. Cross-reference with `mcp__tickvault-logs__run_doctor`.
  3. The 4 boot gates each have a runbook target:
     * Static IP gate → `.claude/rules/dhan/authentication.md` rule 7
     * Dual-instance lock → `docs/error-runbooks/wave-2-error-codes.md`
       (RESILIENCE-01)
     * Clock skew → `docs/error-runbooks/wave-2-c-error-codes.md` (BOOT-03)
     * QuestDB readiness → `docs/error-runbooks/wave-2-error-codes.md`
       (BOOT-01 / BOOT-02)
  4. **No-go below 08:55:** if any gate is still red at 08:55 IST,
     decide whether to skip the trading day. Do NOT force-bypass the
     gate; the gate exists because trading without it is unsafe.

## Section B — One of the 4 boot gates is red

  * **Static IP gate red:** Dhan side `ordersAllowed == false`. Operator
    cannot place orders today. Either skip the day OR call Dhan support
    to re-whitelist (7-day cooldown applies — see
    `.claude/rules/dhan/authentication.md` rule 7).
  * **Dual-instance lock red:** another tickvault process is running
    against the same Dhan client-id. Find + stop the duplicate;
    `mcp__tickvault-logs__docker_status` shows all running containers.
  * **Clock skew red:** host wall-clock drift ≥ 2s vs trusted source.
    Run `chronyc -a 'burst 4/4' && chronyc -a makestep` then restart
    the app.
  * **QuestDB readiness red:** container unreachable. `make doctor`
    section 4 + `docker ps`. If QuestDB cold-starting, wait 30s and
    boot will auto-pass.

## Section C — CloudWatch dashboard widget red

> The local Grafana operator-health dashboard was retired in the
> CloudWatch-only migration (#O1, 2026-05-19). Operator visualization now
> lives in CloudWatch Dashboards; the QuestDB web console covers ad-hoc
> queries.

  1. The dashboard is the CloudWatch `operator-health` dashboard
     (provisioned via Terraform under `deploy/aws/terraform/`).
  2. Each widget maps to a CloudWatch metric (ingested from the app's
     Prometheus-wire-format exporter) — drill in to identify the
     failing dimension.
  3. The 7 widgets: WebSocket connections, QuestDB connected, tick freshness,
     token expiry headroom, spill ring health, Phase 2 outcome,
     composite SLO score.
  4. **Cross-reference with CloudWatch alarms / `mcp__tickvault-logs__run_doctor`**
     (the `list_active_alerts` MCP tool was retired in #O5, 2026-05-30,
     when the Alertmanager container was removed in #O2) — any firing
     alarm names the underlying ErrorCode + runbook path.

## Section D — Subscription count is not 222/222

Expected total: **222 SIDs on the single main-feed connection**
(4 IDX_I + 218 NSE_EQ F&O underlying stocks). Per
`.claude/rules/project/websocket-connection-scope-lock.md`.

  * **Count < 222:** subscription dispatcher dropped some SIDs. Check
    `tv_subscription_acks_received` vs `tv_subscription_requests_sent`.
    Most common cause: Dhan returned an error mid-batch.
  * **Count > 222:** stale subscriptions from a previous boot did not
    unsubscribe cleanly. Restart the app — boot wipes prior state.
  * **Count == 0:** main-feed WebSocket disconnected. Check
    `tv_websocket_connections_active{feed="main"}`. If 0, follow
    `disaster-recovery.md` scenario 6.

## Section E — previous_close table not populated

The bhavcopy loader runs at boot (cold path). Empty for today means:

  1. **The loader ran but bhavcopy was stale:** NSE bhavcopy publishes
     ~17:00 IST the prior trading day. If the loader ran before NSE
     posted, today's `previous_close` rows will be missing. Run
     `make refresh-instruments` to retry.
  2. **The loader failed entirely:** `PREVCLOSE-04` error code in
     `errors.jsonl.*`. See
     `docs/error-runbooks/wave-1-error-codes.md::PREVCLOSE-04`.
     Cascade seal-time pct-stamping falls back to 0.0 for the 3 %
     fields — operator sees zero-valued % columns until next boot.

## What "morning OK" looks like (Telegram reply template)

After all 5 green, send yourself a Telegram message (this becomes your
SEBI-style operator journal entry for the day):

```
Morning OK — <YYYY-MM-DD>
Boot gates: 4/4 ✓
SIDs:       222/222 ✓
prev_close: <N> rows ✓
SLO score:  0.99 ✓
Notes:      <any anomalies observed but resolved>
```

## What "morning NOT OK — skipping day" looks like

If one of the 5 checks did not pass by 09:00 IST, send:

```
SKIPPING <YYYY-MM-DD> — <one-line reason>
Affected gate / tile / count: <specific>
Recovery ETA: <yes/no/unknown>
Operator action: dry_run = true / app off / waiting on Dhan support
```

Then flip `[strategy] dry_run = true` in `config/base.toml` (if not
already), commit + push the change, restart the app, and DO NOT trade.

## Cross-references

  * Behavioral discipline: `docs/runbooks/daily-operations.md`
  * Phase 0 architecture: `.claude/rules/project/phase-0-architecture.md`
  * Operator charter §H: `.claude/rules/project/operator-charter-forever.md`
  * Telegram message style rules: `.claude/plans/friday-may-15-mega/topic-telegram-message-style-rules.md`

## What this runbook does NOT cover

  * **Discipline during trading hours** — see `daily-operations.md`.
  * **Emergency stop mid-session** — deferred until Phase 1 live trading rollout reintroduces the `tickvault-trading` crate.
  * **Post-market shutdown sequence** — see `daily-operations.md`
    "Market Close" section.
