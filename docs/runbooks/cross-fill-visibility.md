# Cross-Fill Visibility — daily digest, table, tile, rollups

> **Operator directive (2026-07-20):** every cross-fill "highlighted, logged,
> monitored, audited, visualised — every day, week, month — precisely at what
> time it is happening."
> **Error code:** CADENCE-04 (`.claude/rules/project/cadence-error-codes.md` §3g).

## What a cross-fill is

Each minute, both broker lanes (Dhan + Groww) fetch their own spot prices +
option chains. When one lane's own path is exhausted and a cell is still
missing, the cadence engine borrows the OTHER broker's same-minute data
(freshness-checked) — that borrow is a **cross-fill**. Separately, when the
Groww burst leaves failed legs, the engine launches Groww's own late refetch —
a **groww_fallback**. Both are self-corrections, not faults — but the operator
must SEE each one, with the exact time.

## Where each event is visible

| Surface | What you see | When |
|---|---|---|
| `cross_fill_audit` QuestDB table | one row per event: minute, lane, source, legs, latency, ladder rung | instantly (best-effort ILP) |
| Coalesced CADENCE-01 log | `stage=cross_fill` / `stage=groww_fallback` per (lane, cycle) | at cycle wrap-up |
| Counters | `tv_cadence_cross_fill_total{direction}`, `tv_cadence_groww_fallback_total{leg}` | instantly |
| `/board` Live Board tile | "Cross-fill today" count + times/directions | ~3s poll |
| Daily Telegram digest | ONE plain-language message | 15:47 IST each trading day |

## What the daily digest means

- `✅ Cross-fill used 0 times today` — every minute's prices came from each
  broker's own feed. Nothing was borrowed. (Sent only when the table actually
  answered the query — never a guessed zero.)
- `🔄 Cross-fill used N time(s) today` + lines like
  `9:16 AM — Dhan spot price filled from Groww` — one line per event, IST
  12-hour times, direction = borrower ← lender.
- `⚠️ Cross-fill check could not run today` — the table was unreadable at
  digest time (CADENCE-04 `stage=digest_read` fired). Count is UNKNOWN, not
  zero. Check QuestDB (`make doctor`), then query the table manually below.

A steady baseline of a few cross-fills around thin minutes (e.g. 09:15/09:16
open) is the engine working as designed. A SPIKE, or every minute of a lane
cross-filling, means that lane's broker surface is degraded — triage via the
CADENCE-01 runbook.

## Table schema (query reference)

`cross_fill_audit` — DEDUP UPSERT KEYS
`(ts, trading_date_ist, lane, cycle_minute_ist, stage)`; `ts` = the decided
minute's OPEN (IST). Columns: `lane` (degraded/borrowing broker),
`source_lane`, `stage` (`cross_fill` | `groww_fallback`), `cycle_minute_ist`
(IST seconds-of-day), `legs` (`spot` | `chain` | `spot+chain`), `rows_filled`,
`cycle_latency_ms` (minute close → event), `ladder_rung`, `resolution`
(`cross_fill` | `native_late_retry`; `native_first_try` reserved for the T+4s
retry PR), `retry_attempts` (0 until the T+4s PR), `resolved_at_ms_after_close`
(`-1` = unknown at launch).

## Rollup SQL (run in the QuestDB console or `mcp__tickvault-logs__questdb_sql`)

**Today, precise times:**

```sql
SELECT ts, lane, source_lane, stage, legs, rows_filled, cycle_latency_ms
FROM cross_fill_audit
WHERE ts >= date_trunc('day', now()) ORDER BY ts;
```

**Daily rollup (last 30 days):**

```sql
SELECT date_trunc('day', ts) AS day, lane, stage, count() AS events,
       sum(rows_filled) AS cells, avg(cycle_latency_ms) AS avg_latency_ms
FROM cross_fill_audit
WHERE ts >= dateadd('d', -30, now())
GROUP BY day, lane, stage ORDER BY day DESC;
```

**Weekly rollup:**

```sql
SELECT date_trunc('week', ts) AS week, lane, stage, count() AS events,
       sum(rows_filled) AS cells
FROM cross_fill_audit
WHERE ts >= dateadd('d', -180, now())
GROUP BY week, lane, stage ORDER BY week DESC;
```

**Monthly rollup:**

```sql
SELECT date_trunc('month', ts) AS month, lane, stage, count() AS events,
       sum(rows_filled) AS cells
FROM cross_fill_audit
GROUP BY month, lane, stage ORDER BY month DESC;
```

**Which minutes cross-fill most (hotspot check):**

```sql
SELECT cycle_minute_ist, count() AS events
FROM cross_fill_audit
WHERE stage = 'cross_fill' AND ts >= dateadd('d', -30, now())
GROUP BY cycle_minute_ist ORDER BY events DESC LIMIT 10;
```

(`cycle_minute_ist` is IST seconds-of-day: 33300 = 9:15 AM, 33360 = 9:16 AM.)

## When something is wrong

- No rows AND no digest on a trading day → check the cadence scheduler is
  enabled (`[cadence]` config) and `mcp__tickvault-logs__tail_errors` for
  CADENCE-04 (`channel` / `append` / `flush` stages = write chain degraded).
- Digest says UNKNOWN → QuestDB was unreadable at 15:47 IST; the events are
  still in the CADENCE-01 logs + counters; re-query the table once QuestDB
  recovers (rows written before the outage are intact).
- The audit is best-effort forensics: it never affects the fetch cadence,
  decisions, or orders.
