# Dual-Feed Scoreboard — Operator Runbook (month-end verdict + daily triage)

> **Purpose:** the operator runs Dhan + Groww live in parallel for a month
> (directive 2026-07-10) and signs a month-end verdict: which feed covered
> more, which was faster, who caused the drops — with every claim backed by
> a QuestDB row, never a feeling.
>
> **Companion rule file (triage):**
> `.claude/rules/project/dual-feed-scoreboard-error-codes.md` (SCOREBOARD-01
> + the full blame taxonomy table).
> **Tables:** `feed_scoreboard_daily` (one row per day per feed),
> `feed_episode_audit` (one row per disconnect/stall/process-death episode,
> blame persisted), `feed_coverage_daily` (per-instrument detail — populated
> when the presence registry ships, PR-4).
> **Daily Telegram:** "📊 Daily feed scorecard @ 3:45 PM IST" (Info,
> immediate). Its absence is itself paged ("Daily feed scorecard did NOT
> run", High).

---

## 1. Table / column quick reference

`feed_scoreboard_daily` — DEDUP `(ts, trading_date_ist, feed)`; `ts` is the
DETERMINISTIC trading-date 15:45:00 IST stamp, so re-runs UPSERT in place.

| Column group | Columns | Notes |
|---|---|---|
| Coverage | `ticks_captured`, `instruments_seen`, `unique_win_minutes`, `both_minutes`, `streaming_minutes`, `session_minutes` (375), `uptime_pct` | `-1` = source unavailable that run (NEVER a fabricated 0) |
| Per-instrument (PR-4) | `mapped_instruments`, `unmapped_instruments`, `covered_instrument_minutes` | `-1` until the presence registry ships |
| Lag (PR-3) | `lag_p50_ms`, `lag_p99_ms`, `lag_max_ms`, `lag_samples`, `lag_floor_ms` | `-1` until the day histograms ship; `lag_floor_ms` is the honesty column (Dhan 1000 — whole-second price clock; Groww 1) |
| Episodes | `disconnects_market`, `disconnects_off_hours`, `reconnects`, `stalls`, `blame_broker`, `blame_ours`, `blame_indeterminate`, `restarts_detected` | blame tallies exclude off-hours rows |
| Honesty | `partial_coverage`, `coverage_source` (`in_memory`/`sql_backfill`/`mixed`), `outcome` (`complete`/`partial`/`degraded`) | `degraded` = the connection-event record itself under-counted that day |

`feed_episode_audit` — DEDUP
`(ts, trading_date_ist, feed, ws_type, connection_index, episode_kind)`.
Columns: `blame` (broker/ours/indeterminate — never blank), `blame_reason`
(machine slug), `source`, `dhan_code` (−1 = none), `detector`
(`ws_event_audit` / `stall_row` / `boot_reconciled`), `down_secs`,
`market_hours`, `evidence` (≤200 chars, secret-redacted), `run_partial`.

## 2. Month-end cumulative verdict SQL

Run via `mcp__tickvault-logs__questdb_sql` (or the QuestDB console):

```sql
SELECT feed, count() days, sum(unique_win_minutes) unique_mins, sum(ticks_captured) ticks,
       avg(lag_p50_ms) p50_ms, avg(lag_p99_ms) p99_ms, max(lag_max_ms) worst_ms,
       sum(disconnects_market) drops, sum(blame_broker) broker, sum(blame_ours) ours,
       sum(blame_indeterminate) unclear, sum(stalls) stalls, avg(uptime_pct) uptime,
       sum(case when partial_coverage then 1 else 0 end) partial_days
FROM feed_scoreboard_daily
WHERE trading_date_ist >= '2026-07-01' AND trading_date_ist < '2026-08-01' GROUP BY feed;
```

Caveat: while lag columns are `-1` sentinels (pre-PR-3 days), `avg(lag_*)`
is polluted by the sentinels — exclude them:

```sql
SELECT feed, avg(lag_p99_ms) p99_ms FROM feed_scoreboard_daily
WHERE lag_p99_ms >= 0 AND trading_date_ist >= '2026-07-01'
  AND trading_date_ist < '2026-08-01' GROUP BY feed;
```

Per-day winner drill (tiebreak = lag_p99 where measured):

```sql
SELECT d.trading_date_ist, d.unique_win_minutes dhan_mins, g.unique_win_minutes groww_mins,
       d.lag_p99_ms dhan_p99, g.lag_p99_ms groww_p99,
       d.blame_broker dhan_broker_drops, g.blame_broker groww_broker_drops
FROM (SELECT * FROM feed_scoreboard_daily WHERE feed = 'dhan') d
JOIN (SELECT * FROM feed_scoreboard_daily WHERE feed = 'groww') g
  ON d.trading_date_ist = g.trading_date_ist
ORDER BY d.trading_date_ist;
```

Per-instrument worst offenders (from PR-4 onward):

```sql
SELECT symbol_name, exchange_segment, sum(dhan_only_minutes) dhan_only,
       sum(groww_only_minutes) groww_only, sum(both_minutes) both
FROM feed_coverage_daily
WHERE trading_date_ist >= '2026-07-01' AND trading_date_ist < '2026-08-01'
GROUP BY symbol_name, exchange_segment
ORDER BY dhan_only + groww_only DESC LIMIT 25;
```

## 3. Indeterminate-review procedure (MANDATORY before signing the month)

Blame is evidential, not proof. The month verdict weighs `broker` only —
and the operator MUST review every `indeterminate` episode first:

```sql
SELECT ts, feed, ws_type, episode_kind, blame_reason, source, dhan_code,
       down_secs, evidence, run_partial
FROM feed_episode_audit
WHERE blame = 'indeterminate'
  AND trading_date_ist >= '2026-07-01' AND trading_date_ist < '2026-08-01'
ORDER BY ts;
```

Review guide per `blame_reason`:

| reason | What it means | How to review |
|---|---|---|
| `transport_ambiguous` | Mid-stream reset with no same-day WS-GAP-09 corroboration | Cross-check the exact minute in `data/logs` / CloudWatch: other Dhan calls failing at the same time ⇒ network (ours-ish); only the feed dropped ⇒ Dhan-side |
| `network_path` | Handshake / TLS / DNS / refused / timeout | Box egress + DNS health at that minute |
| `unknown_cause` / `unclassified` | Novel error text / novel source label | Read the `evidence` column; if a pattern repeats, extend the classifier (a dated PR) |
| `off_hours_idle` | Pre/post-market idle cleanup | Expected noise — excluded from headline counts; no action |
| `run_partial = true` rows | Classified with EXPIRED errors.jsonl evidence (>48h backfill) | 805s defaulted broker, RSTs defaulted indeterminate — annotate rather than re-litigate |

An `outcome = 'degraded'` day (audit-row drops) means that day's episode
counts are a FLOOR — annotate the month summary accordingly:

```sql
SELECT trading_date_ist, feed, outcome, partial_coverage
FROM feed_scoreboard_daily WHERE outcome != 'complete' ORDER BY trading_date_ist;
```

## 4. Backfill / re-run

- **Re-run today:** restart with `TICKVAULT_SCOREBOARD_NOW=1`. Idempotent —
  the daily row carries the deterministic 15:45 IST ts and the episode rows
  reuse their audit-row ts, so everything UPSERTs in place.
- **What a re-run/backfill CANNOT recover:**
  - errors.jsonl correlation older than the 48h retention → 805 episodes
    default `broker`, resets default `indeterminate` (rows flagged
    `run_partial`).
  - Lag histograms for a past day (PR-3 onward they are in-memory,
    per-process) → sentinels stay.
  - Per-instrument unique-wins for pre-PR-4 days.
  - Stall episode rows for days before the stall event kind ships (PR-2) —
    those days honestly read `stalls = 0`; the CloudWatch counter
    `tv_feed_sidecar_stall_restart_total` holds the past.
- **Process deaths** are reconciled once per boot (≈3 min after start). A
  death is only synthesized when the boot happened in-session AND this
  boot's own connect row landed — the 16:30 auto-stop → 08:30 start cycle
  never counts as a death.

## 5. Day-1 notes (PR-A scope)

- Disconnect episodes + blame + per-feed coverage totals + feed-level
  exclusive/both minutes work from day 1 (they aggregate the EXISTING
  `ws_event_audit` + `ticks` tables — any past day is backfillable for
  those).
- Lag = `-1` sentinels until PR-3; per-instrument detail until PR-4;
  stall rows until PR-2. The Telegram footnotes say so explicitly.
- The whole subsystem is toggleable: `[scoreboard] enabled = false` in
  `config/base.toml` spawns nothing (the rollback switch).
- Failure signal: SCOREBOARD-01 in the error stream
  (`mcp__tickvault-logs__tail_errors`), plus the High "scorecard did NOT
  run" Telegram on task death. SCOREBOARD-01 is log-sink-only (no
  CloudWatch alarm — budget exhausted; the daily Telegram is the signal).

## 6. Honesty checklist (attach to any month-verdict statement)

- [ ] Every `indeterminate` episode reviewed (§3) — or explicitly counted
      as "unclear", never folded into a side.
- [ ] `partial_days` and `degraded` days named alongside the totals.
- [ ] Lag comparison excludes `-1` sentinels AND states the Dhan
      whole-second floor (`lag_floor_ms` 1000 vs 1) — "Groww faster by
      under a second" is UNPROVABLE against Dhan's floor.
- [ ] Coverage claims say "at our capture boundary" — a unique-win minute
      means the other feed delivered nothing THAT WE CAPTURED, not proof of
      what the exchange traded (both vendors sample).
- [ ] Stall counts note the PR-2 ship date (earlier days read 0).
