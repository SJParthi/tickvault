# Phase 1 Monitoring Rubric — 22-Day Dry-Run Pass/Fail Criteria

> **Authority:** SEBI compliance + operator-charter §H + Phase 0 architecture pillar 4.
> **Scope:** The 22-day observation window between Phase 0 boot-gate completion and the `[strategy] dry_run = false` flip.
> **Decision authority:** Parthiban (architect). Claude Code presents data; operator decides go/no-go.
> **Source of truth:** This file is the ONLY rubric. Anyone proposing additional criteria opens a PR amending this file FIRST.

## Why 22 days

22 trading days ≈ 1 calendar month. Captures:

  * 4 weekly expiry rollovers (one per week)
  * 1 monthly expiry rollover
  * At least 1 RBI policy event window OR equivalent volatility event
  * Enough Telegram-alert volume to characterize false-positive rate
  * Enough audit-table rows to verify SEBI retention chain works end-to-end
  * Enough sustained-mid-session data to detect tick-gap pattern (anomaly vs systemic)

Shorter windows (e.g. 5-day) miss rare-but-deadly conditions (expiry day,
holiday wake, weekend cold start). Longer windows (e.g. 60-day) waste
operator attention on what is essentially the same week 4 times.

## The 8 mandatory PASS criteria

Phase 1 promotion is BLOCKED unless ALL 8 criteria pass simultaneously
on the SAME 22-day window. Mixing windows ("good week 3 but bad week 5
so let's go") is FORBIDDEN.

### 1. Disconnect rate — main feed

| Window | Pass threshold | Source |
|---|---|---|
| Per trading day | ≤ 5 disconnects | `tv_websocket_disconnects_total{feed="main"}` |
| Per 22-day window | ≤ 60 cumulative disconnects | same |
| Hard fail | Any single 4h period with ≥ 10 disconnects | edge-triggered |

**Rationale:** SEBI 24h JWT forces ≥1 reconnect per day BY LAW. 5/day
allows for that + post-close sleep transition + occasional Dhan-side
RST. Above this, network or auth instability is real.

**Inspection:** read `tv_websocket_disconnects_total{feed='main'}` from CloudWatch metrics (prod) or the app `/metrics` endpoint (dev). The `prometheus_query` MCP tool was retired in #O5 (2026-05-30) — Prometheus container removed in #O3.

### 2. Disconnect rate — order update

| Window | Pass threshold |
|---|---|
| Per trading day | ≤ 3 disconnects |
| Per 22-day window | ≤ 30 cumulative disconnects |

**Rationale:** Order-update WS only matters when orders are placed.
Phase 0 has no live orders, but the connection MUST stay alive — a
broken order-update WS in Phase 1 means we'd place orders blind.

### 3. Gap-fill success rate

| Metric | Pass threshold |
|---|---|
| `tv_gap_fill_success_total / tv_gap_fill_attempts_total` | ≥ 99.0 % |
| Gap-fill attempts per trading day | ≤ 10 |
| Single gap > 60s | 0 occurrences |

**Rationale:** Gap-fill is the safety net for missed ticks. If we
routinely need it, the live feed is unhealthy. If it fails when we
need it, we have silent data loss.

**Source:** `tv_gap_fill_*` counters (PR-track for Items 8 + 9).

### 4. Cross-verify match rate

| Metric | Pass threshold |
|---|---|
| Daily cross-verify (live ticks → 1m candles vs Dhan REST) | ≥ 99.5 % candles match exactly |
| Mismatches per trading day | ≤ 5 |
| Mismatches sustained > 3 consecutive days | HARD FAIL |

**Rationale:** Cross-verify is the proof that our pipeline produces
the same data as Dhan. Below 99.5% means parser bugs or persistence
bugs are corrupting the data.

**Source:** `tv_cross_verify_*` counters + cross-verify Telegram digest.

### 5. Signal fire rate within expected range

| Strategy | Expected daily signal count | Tolerance |
|---|---|---|
| (To be populated by strategy-config PR; placeholder) | TBD | ±30% |

**Rationale:** If signals fire 10× expected, the strategy is over-firing
(probably a parameter regression). If signals fire 0×, the strategy is
broken or the feed is broken.

**Source:** `signal_audit` table query — group by `strategy_id` and
`signal_name`, count by `trading_date_ist`.

### 6. No NaN/Inf indicator resets

| Metric | Pass threshold |
|---|---|
| `tv_indicator_nan_guard_fired_total` per 22-day window | = 0 |

**Rationale:** The NaN guard is a defensive clamp, not a "feature we
expect to use". If it ever fires, we have an upstream bug in the
indicator engine. Any non-zero count blocks Phase 1.

**Source:** Item 21 (`IndicatorSnapshot::sanitize_nan_inf`).

### 7. No orphan positions

| Metric | Pass threshold |
|---|---|
| `tv_orphan_positions_detected_total` per 22-day window | = 0 |

**Rationale:** In Phase 0 dry-run we don't have positions, but the
detector still runs against synthetic state. In Phase 1, any orphan
position is a real money problem.

**Source:** Item 20 (15:25 IST orphan watchdog, pending).

### 8. Composite SLO score

| Metric | Pass threshold |
|---|---|
| Daily composite SLO score floor | ≥ 0.95 every trading day at 15:30 IST |
| Critical-band entries (score < 0.80) per 22-day window | = 0 |

**Rationale:** The composite SLO is the single-glance answer to "is
the system working?" Below 0.95 on any day means a dimension was
degraded enough to surface as SLO-02. Below 0.80 = Critical.

**Source:** `tv_slo_composite_score` gauge + SLO-01/SLO-02 events.

## Telegram alert volume budget

Separate from PASS criteria, this is operator-attention budget:

| Severity | Daily budget | 22-day budget |
|---|---|---|
| Critical | ≤ 1 / day | ≤ 5 / window |
| High | ≤ 5 / day | ≤ 60 / window |
| Medium | (no budget — informational) | — |
| Low / Info | (no budget — daily heartbeats expected) | — |

Above the High/day budget, the operator becomes desensitized.
If Phase 0 sustains High > 5/day, the alert thresholds need tuning
BEFORE Phase 1, not after.

## Audit-table SEBI verification (one-time, end of window)

At the end of the 22-day window, before promoting to Phase 1:

  1. **Row count sanity:** every audit table has ≥ 22 entries (one per
     trading day minimum).
     ```sql
     SELECT count_distinct(trading_date_ist) FROM <each_audit_table>;
     ```
     Must equal 22 (or close — accounting for holidays).
  2. **DEDUP correctness:** no two rows for the same composite key.
     ```sql
     SELECT trading_date_ist, security_id, exchange_segment, outcome, count(*)
     FROM <table>
     GROUP BY trading_date_ist, security_id, exchange_segment, outcome
     HAVING count(*) > 1;
     ```
     Must return 0 rows.
  3. **Retention proof:** the oldest row should match the boot date.
     ```sql
     SELECT min(ts), max(ts) FROM boot_audit;
     ```
     Spread should be ≥ 22 trading days.

If any of the 3 SEBI checks fail, Phase 1 is BLOCKED until the audit
chain is fixed.

## Cross-verify with `prev_close` semantics (post-Item 26 L3 verdict)

Phase 1 promotion is BLOCKED until Item 26 L3 (the Dhan volume-monotonicity
escalation) reaches verdict. Either:

  * Dhan confirms volume is cumulative → continue with Phase 1.
  * Dhan confirms volume semantic changed → patch parser + re-run
    the 22-day window.
  * Dhan does not respond within 14 days → operator decides based on
    the L1 + L2 cross-check results.

## How to declare PASS or FAIL

At the end of the 22-day window:

```
.claude/plans/friday-may-15-mega/topic-PHASE-1-RUBRIC-VERDICT.md
```

This file is created by the operator (NOT Claude Code) and contains:

  * Window start/end dates (YYYY-MM-DD)
  * Each of the 8 criteria with actual measured value vs threshold
  * Audit-table SEBI verification SQL outputs
  * Operator's verdict: PASS / FAIL / EXTEND
  * If FAIL: which criterion failed + remediation plan
  * If EXTEND: rationale + extended window length

The verdict file becomes part of the git history. Phase 1 promotion
commits MUST cite the verdict file's commit hash.

## What this rubric does NOT cover

  * **Profitability** — Phase 1 is about safety + correctness, not
    P&L. P&L is evaluated AFTER 30 days of live trading.
  * **Latency budgets** — covered by `quality/benchmark-budgets.toml`
    and the bench-gate.
  * **Test coverage** — covered by `quality/crate-coverage-thresholds.toml`
    and the coverage-gate.
  * **Strategy alpha** — backtest-runner runbook (`27f`, pending) is
    the alpha-validation workflow.

## Cross-references

  * Phase 0 architecture: `.claude/rules/project/phase-0-architecture.md`
  * Operator daily startup: `docs/runbooks/operator-daily-startup.md`
  * Daily discipline: `docs/runbooks/daily-operations.md`
  * Troubleshooting (per ErrorCode): `docs/runbooks/troubleshooting.md`
  * SLO score: `docs/error-runbooks/wave-3-d-error-codes.md::SLO-02`
  * Operator charter: `.claude/rules/project/operator-charter-forever.md`

## The honest 100% claim (for Phase 1 promotion only)

> "100% inside the tested envelope, with ratcheted regression coverage:
> 22-day disconnect rate, gap-fill success, cross-verify match, NaN
> guard, orphan position, composite SLO — ALL 8 PASS criteria satisfied
> simultaneously on the same 22-day window. Beyond the envelope, this
> rubric does NOT predict future market behavior or profitability — it
> proves the SYSTEM is safe to trust with capital, not that the
> STRATEGY will make money."
