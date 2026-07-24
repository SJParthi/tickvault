# Dual-Feed Scoreboard — Operator Runbook (month-end verdict + daily triage)

> **⚠ SUPERSEDED IN PURPOSE 2026-07-13 (operator directive — Dhan live WS retired):** the
> 2026-07-10 month-long Dhan-vs-Groww live comparison is CONCLUDED EARLY by the 2026-07-13
> retirement directive (verbatim + evidence record: `websocket-connection-scope-lock.md`
> "2026-07-13 Amendment" §E — the comparison's own findings, p99 46.37s vs 562ms lag +
> 29–67 silent SIDs/min + the post-#1474 candle mismatches, are WHY the Dhan feed was
> retired). From 2026-07-13 the scorecard is a **GROWW-monitoring scorecard + Dhan REST
> cross-checks** (§37 BruteX / §38 per-minute REST parity), not a two-feed contest:
> **Dhan rows legitimately read `feed_off` / absent — "Dhan live feed disabled by operator
> 2026-07-13" — never an outage implication.** The `feed_off` day detection (round 4/5/6
> semantics below) already handles this honestly: the pre-session disable state stamps
> `feed_off`, the partner feed's `unique_win_minutes`/`both_minutes` read the −1 sentinel
> ("exclusive-vs-nothing is not a measurement"), and the month SQL's no-contest exclusion
> applies. The §2 month-end cumulative VERDICT SQL is retained for the ALREADY-CAPTURED
> 2026-07-10..12 dual-feed days (historical audit) and for a future GDF-vs-Groww month
> (`gdf-third-feed-scope-2026-07-13.md`); do not sign a "month verdict" spanning the
> retirement boundary without splitting at 2026-07-13. Tables, blame taxonomy, and the
> daily 15:45 IST Telegram are UNCHANGED.
>
> **⚠ 2026-07-15 dormancy note:** the Groww LIVE machinery this runbook's stall/bridge/
> sidecar triage sections describe (sidecar, bridge, stall watchdog, lag publisher) was
> DELETED with the Groww live feed (operator directive 2026-07-15 — REST-only for both
> brokers). Those sections are HISTORICAL triage for already-captured rows; there is no
> sidecar to restart anymore.

> **Purpose:** the operator runs Dhan + Groww live in parallel for a month
> (directive 2026-07-10) and signs a month-end verdict: which feed covered
> more, which was faster, who caused the drops — with every claim backed by
> a QuestDB row, never a feeling.
>
> **Companion rule file (triage):**
> `docs/error-runbooks/dual-feed-scoreboard-error-codes.md` (SCOREBOARD-01
> + the full blame taxonomy table).
> **Tables:** `feed_scoreboard_daily` (one row per day per feed),
> `feed_episode_audit` (one row per disconnect/stall/process-death episode,
> blame persisted), `feed_coverage_daily` (per-instrument detail — POPULATED
> since PR-D, 2026-07-11: one row per instrument per day from the in-memory
> presence registry; config-gated `[scoreboard] coverage_detail_rows`).
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
| Per-instrument (MEASURED since PR-D, 2026-07-11) | `mapped_instruments`, `unmapped_instruments`, `covered_instrument_minutes` | Drained from the in-memory presence registry on same-day runs (per-instrument 375-minute bitsets; cross-feed slots paired by ISIN / canonical index name / future contract identity); `-1` = not measured (pre-ship days, past-day backfills — the registry is process-local + day-scoped). Since PR-D, `unique_win_minutes`/`both_minutes` are ALSO registry truth when `coverage_source` reads `in_memory`; on `mixed` days those two comparison columns keep the SQL minute-set values (fix round 1 — see the §6 checklist bullet) while the per-instrument mapped/unmapped/covered columns take the registry's partial measurement |
| Lag (MEASURED since PR-C, 2026-07-11) | `lag_p50_ms`, `lag_p99_ms`, `lag_max_ms`, `lag_samples`, `lag_floor_ms` | Drained from the in-memory per-feed DAY histograms on same-day runs (exchange→receipt, replay/re-tail excluded at record time); `-1` = not measured (pre-ship days, past-day backfills — the histograms are process-local + day-scoped — or a thin <50-sample day); `lag_floor_ms` is the honesty column (Dhan 1000 — whole-second price clock, p50/p99 can never read sub-second; Groww 1 — millisecond clock, but receipt = the sidecar capture instant one hop downstream of the socket, so the two feeds' clocks are NOT like-for-like) |
| Episodes | `disconnects_market`, `disconnects_off_hours`, `reconnects`, `stalls`, `blame_broker`, `blame_ours`, `blame_indeterminate`, `restarts_detected` | blame tallies exclude off-hours rows |
| Honesty | `partial_coverage`, `coverage_source` (`in_memory`/`sql_backfill`/`mixed`), `outcome` (`complete`/`partial`/`degraded`/`feed_off`) | `degraded` = the connection-event record itself under-counted that day; `feed_off` = the feed was switched off for the day (one-horse race — EXCLUDED from the month sums, round 4) |

`feed_episode_audit` — DEDUP
`(ts, trading_date_ist, feed, ws_type, connection_index, episode_kind)`.
Columns: `blame` (broker/ours/indeterminate — never blank), `blame_reason`
(machine slug), `source`, `dhan_code` (−1 = none), `detector`
(`ws_event_audit` / `stall_row` / `boot_reconciled`), `down_secs`,
`market_hours`, `evidence` (≤200 chars, secret-redacted), `run_partial`.

`down_secs` semantics per detector: `ws_event_audit` rows carry the REAL
reconnect downtime from the audit row; `detector='boot_reconciled'`
(process-death) rows record **0 = unknown** — the last-audit-row-to-
reconnect gap is only an UPPER BOUND (audit rows are edge-triggered, so
the prior row can be hours old) and lives in the `evidence` string, never
in the column. NEVER sum `down_secs` across detectors as "total downtime".

Headline-tally scope: the per-day `disconnects_*` / `reconnects` / blame /
`restarts_detected` columns count MARKET-DATA channels only (`main_feed`,
`groww_bridge`) — the Dhan order-update trading channel's episodes are
persisted in `feed_episode_audit` with their `ws_type` for forensics but
are EXCLUDED from the Dhan-vs-Groww comparison (a dead-token day once put
39+ order-update drops on Dhan's card while its market feed was perfect).

## 2. Month-end cumulative verdict SQL

Run via `mcp__tickvault-logs__questdb_sql` (or the QuestDB console).

**Sentinel rule (applies to EVERY LONG column):** a partial day persists
the `-1` "source unavailable" sentinel into any column it could not
measure — a naive `sum()` SUBTRACTS 1 per sentinel day and silently
corrupts the month totals (it under-reports drops on exactly the feed
that had the outage day). Every summed column therefore carries a
per-column guard, plus a `*_days` measured-day count so a low total from
few measured days is never misread as a good month:

```sql
SELECT s.feed, count() days,
       sum(case when s.unique_win_minutes >= 0 then s.unique_win_minutes else 0 end) unique_mins,
       sum(case when s.unique_win_minutes >= 0 then 1 else 0 end) unique_mins_days,
       sum(case when s.ticks_captured >= 0 then s.ticks_captured else 0 end) ticks,
       sum(case when s.ticks_captured >= 0 then 1 else 0 end) ticks_days,
       max(s.lag_max_ms) worst_ms,
       sum(case when s.disconnects_market >= 0 then s.disconnects_market else 0 end) drops,
       sum(case when s.disconnects_market >= 0 then 1 else 0 end) drops_days,
       sum(case when s.blame_broker >= 0 then s.blame_broker else 0 end) broker,
       sum(case when s.blame_ours >= 0 then s.blame_ours else 0 end) ours,
       sum(case when s.blame_indeterminate >= 0 then s.blame_indeterminate else 0 end) unclear,
       sum(case when s.stalls >= 0 then s.stalls else 0 end) stalls,
       avg(s.uptime_pct) uptime,
       sum(case when s.partial_coverage then 1 else 0 end) partial_days
FROM feed_scoreboard_daily s
LEFT JOIN (
    SELECT DISTINCT trading_date_ist FROM feed_scoreboard_daily
    WHERE outcome = 'feed_off'
      AND trading_date_ist >= '2026-07-01'
      AND trading_date_ist < '2026-08-01') off
  ON s.trading_date_ist = off.trading_date_ist
WHERE off.trading_date_ist IS NULL
  AND s.trading_date_ist >= '2026-07-01' AND s.trading_date_ist < '2026-08-01'
GROUP BY s.feed;
```

**⚠ QuestDB SQL dialect (round 6, 2026-07-10 — verified live on the pinned
9.3.5):** the engine does NOT support `IN (SELECT …)` / `NOT IN (SELECT …)`
subqueries against a TIMESTAMP column — it fails with
`cannot compare TIMESTAMP with type CURSOR` (IN accepts only literal
lists). The round-5 form of this query used `trading_date_ist NOT IN
(SELECT …)` and errored on the first copy-paste. The LEFT JOIN
anti-join above (`LEFT JOIN (SELECT DISTINCT …) off ON s.trading_date_ist
= off.trading_date_ist WHERE off.trading_date_ist IS NULL`) is the
execution-verified replacement shape — do NOT "simplify" it back to a
`NOT IN` subquery in a future edit. (A two-step fallback also works:
first `SELECT DISTINCT trading_date_ist … WHERE outcome='feed_off'`, then
paste the dates as a literal `NOT IN ('…','…')` list.)

**The feed_off exclusion is DAY-level, not row-level (round 5,
2026-07-10):** a row-level `outcome != 'feed_off'` filter removes only the
OFF feed's row while the SURVIVING feed's row for the same day still sums
— its numbers are one-horse (no competitor to lose a minute to). The
anti-join above drops the WHOLE no-contest day for BOTH feeds,
which is what the daily card's "this day does not count toward the month
verdict" footnote promises. Belt-and-braces: since round 5 the writer also
stamps the surviving feed's `unique_win_minutes`/`both_minutes` with the
`-1` sentinel on a feed-off day (exclusive-vs-nothing is not a
measurement), so even a query that forgets the anti-join no longer inflates
`unique_mins` — but ticks/uptime/blame on the partner row remain one-horse
context, hence the day-level exclusion stays the documented form.

Caveats:
- `avg(lag_*)` is polluted by the `-1` sentinels (pre-PR-C days,
  backfilled days, thin days) — compute lag averages ONLY with a
  `WHERE lag_p99_ms >= 0` variant, and NEVER compare Dhan-vs-Groww lag
  below Dhan's 1s floor (`lag_floor_ms`; "Groww faster by <1s" is
  unprovable against Dhan's whole-second clock). The card's verdict
  delay rung enforces this mechanically since review round 1
  (2026-07-11): it declares a lag winner only when the cross-feed p99
  delta EXCEEDS 1000 ms ("faster prices beyond the clock floor");
  sub-floor deltas fall through — apply the same discipline to any
  manual month-SQL comparison:

```sql
SELECT feed, avg(lag_p99_ms) p99_ms FROM feed_scoreboard_daily
WHERE lag_p99_ms >= 0 AND trading_date_ist >= '2026-07-01'
  AND trading_date_ist < '2026-08-01' GROUP BY feed;
```

- **Groww drop/blame sums are a FLOOR for every pre-PR-B day (before
  2026-07-10):** Groww disconnect rows exist only for feed-disable and
  bridge-death, and the dominant sidecar socket-drop family wrote no
  episode row until the stall event kind shipped (PR-B, 2026-07-10) —
  `drops`/`broker`/`stalls` for Groww under-count those days. Do NOT read
  "Dhan N vs Groww 0 drops/stalls" as a Groww win for that period. FROM
  PR-B forward the socket-death family lands in the `stalls` column
  (blame-attributed via the `stall_*` source slugs) and the card renders
  Groww drops + stalls as measurements. **Permanent asymmetry (honest):**
  the stalls column is Groww-only by construction — the Dhan main-feed
  has no sidecar; its silent-socket detection (WS-GAP-06 / activity
  watchdog) reconnects in-process and is already counted in Dhan's
  disconnect/reconnect rows. Compare INCIDENT TOTALS (drops + stalls +
  restarts — the card's blame split already covers all three), never the
  stalls column across feeds. Residual on BOTH feeds: in-sidecar /
  in-process reconnects that recover faster than the 30s stall threshold
  write no row anywhere.
- `avg(uptime_pct)` includes partial days (their 0.0-with-partial-flag
  rows) — cross-check `partial_days` before quoting it.
- **`outcome = 'feed_off'` days are EXCLUDED above (round 4, 2026-07-10):**
  a day the operator ran dhan-only / groww-only (the /api/feeds toggle or a
  single-feed profile) measures REAL zeros for the off feed — folding them
  into the sums (or letting the card crown the other feed on 375 exclusive
  minutes) skews the exact verdict this runbook exists to sign. Report them
  separately, and name them next to the month totals:

```sql
SELECT trading_date_ist, feed FROM feed_scoreboard_daily
WHERE outcome = 'feed_off'
  AND trading_date_ist >= '2026-07-01' AND trading_date_ist < '2026-08-01'
ORDER BY trading_date_ist;
```

  Detection (redesigned round 5, hardened round 6, 2026-07-10):
  measured-zero ticks + zero up-kind connection rows INSIDE the session
  window ([09:00, 15:30) IST — the ~08:33 boot Connected row every
  config-enabled feed writes no longer defeats the runtime-disable day; a
  WS-GAP-04 wake's ~09:00:00 SleepResumed row that the dormant gate
  immediately re-parked with a `feed_disabled` marker within seconds is
  machinery, not up evidence, and is excluded too — the
  disabled-while-sleeping overnight shape) + either NO up rows at all
  (config-off) or a PRE-session `source='feed_disabled'` toggle row that
  is the feed's STATE AT SESSION OPEN — i.e. the LAST pre-session toggle
  (round 6: a disable→re-enable flap, 08:40 off / 08:50 back on, does NOT
  qualify — the broker-dead session after it stays a loud catastrophic
  measured zero). A boot up row WITHOUT a disable-at-open state is an
  ENABLED-but-dead-broker day — never softened into feed_off (the live
  runtime flag additionally blocks the NO-marker arm on same-day runs).
  The durable state-at-open marker OUTRANKS the run-instant flag (round
  6): re-enabling the day-long-disabled feed for tomorrow between 15:30
  and the 15:45 trigger no longer stamps the day complete-with-zeros. An
  existing feed_off row is also keep-better-protected: a same-day evening
  rerun with the feed re-enabled for tomorrow (zero ticks re-measured)
  keeps feed_off and logs `stage="outcome_regression"`; a rerun that
  measured REAL ticks may upgrade. Honest residuals: (a) on a PAST-day
  backfill the inference is data-only — an enabled feed that never
  achieved a single successful connect ALL day (zero rows entirely) is
  indistinguishable from config-off; (b) a Groww disable that lands while
  the bridge is ALREADY disconnected writes NO `feed_disabled` marker (the
  falling-edge row is latched on a prior connected state), so that day
  reads catastrophic broker-dead, not feed_off — conservative direction
  (stays loud, never softens); (c) a pre-session disable→re-enable where
  the re-enable produces NO up row (broker already dead at the re-enable
  instant) still reads state-at-open OFF and classifies feed_off —
  narrow, and only reachable via a same-morning flap onto a dead broker.

Per-day winner drill (tiebreak = lag_p99 where measured; the same
day-level feed_off exclusion applies — a no-contest day would otherwise
join here as a `-1`-vs-0 line; same anti-join shape as §2's month verdict
— QuestDB rejects `NOT IN (SELECT …)` on TIMESTAMP):

```sql
SELECT d.trading_date_ist, d.unique_win_minutes dhan_mins, g.unique_win_minutes groww_mins,
       d.lag_p99_ms dhan_p99, g.lag_p99_ms groww_p99,
       d.blame_broker dhan_broker_drops, g.blame_broker groww_broker_drops
FROM (SELECT * FROM feed_scoreboard_daily WHERE feed = 'dhan') d
JOIN (SELECT * FROM feed_scoreboard_daily WHERE feed = 'groww') g
  ON d.trading_date_ist = g.trading_date_ist
LEFT JOIN (
    SELECT DISTINCT trading_date_ist FROM feed_scoreboard_daily
    WHERE outcome = 'feed_off') off
  ON d.trading_date_ist = off.trading_date_ist
WHERE off.trading_date_ist IS NULL
ORDER BY d.trading_date_ist;
```

Per-instrument worst offenders (rows exist from PR-D, 2026-07-11 onward;
`mapped = false` singleton rows carry `-1` comparison sentinels — the
`mapped = true` filter below skips them):

```sql
SELECT symbol_name, exchange_segment, sum(dhan_only_minutes) dhan_only,
       sum(groww_only_minutes) groww_only, sum(both_minutes) both
FROM feed_coverage_daily
WHERE trading_date_ist >= '2026-07-01' AND trading_date_ist < '2026-08-01'
  AND mapped = true
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
| `run_partial = true` rows | Classified with EXPIRED errors.jsonl evidence (>48h backfill; round 4 made this HORIZON-aware — a readable log dir whose retained files cannot COVER the target day counts as partial evidence too, so the flag can no longer lie false on aged-out backfills) | 805s defaulted broker, RSTs defaulted indeterminate — annotate rather than re-litigate (a re-run can NEVER downgrade an evidence-backed row — the keep-better guard suppresses it and logs `stage="blame_regression"`) |
| `post_close_restart` | The reconnect landed AFTER 3:30 PM close AND the feed had streamed through ~15:28 (round 4: a feed whose last streamed minute leaves a HOLE before close re-classifies as a REAL in-market death instead — counted, floor engaged; only streamed-through-close keeps this carve-out, and a failed minute read keeps it loudly via `stage="post_close_disambiguation"`). **Known residual (round 5): the silent-tail false positive** — a feed whose broker went tick-silent before ~15:28 with the socket UP (server pings keep the activity watchdog quiet, so NO disconnect row is written) that was then CLEANLY auto-stopped at 16:30 and evening-restarted produces the SAME data shape and re-classifies as an in-market `ours/process_restart` for a process that demonstrably survived to the scheduled stop. The error direction is self-blame inflation (conservative for the month verdict), but the row is factually wrong — before signing such a row, cross-check the day's tick-gap (WS-GAP-06) / FEED-STALL signals for a broker-silence tail and whether the OTHER feed streamed through close on the same process | Forensic row only — excluded from the headline restarts/blame counts and the partial floor; no action unless it repeats on days with NO evening restart |

An `outcome = 'degraded'` day (audit-row drops) means that day's episode
counts are a FLOOR — annotate the month summary accordingly:

```sql
SELECT trading_date_ist, feed, outcome, partial_coverage
FROM feed_scoreboard_daily WHERE outcome != 'complete' ORDER BY trading_date_ist;
```

## 4. Backfill / re-run

- **Post-close restarts DON'T duplicate the card (round 4; round 5 made
  it work on single-feed days):** a same-day boot past the trigger whose
  day ALREADY carries a TERMINAL row for BOTH feeds — `complete` OR
  `feed_off` (a rerun cannot "improve" a feed-off day, and on a
  single-feed profile EVERY day carries one feed_off row, so requiring
  complete-on-both had left the latch permanently dead there) — skips the
  catch-up re-run and sends no duplicate Telegram; partial/degraded days
  still re-run, and a boot that detected an in-market crash never skips
  (the restart floor must land). A rerun can also never ERASE a
  `degraded` OR `feed_off` verdict — the daily keep-better keeps it and
  logs `stage="outcome_regression"` (a rerun that measured REAL ticks may
  upgrade a feed_off row: the feed genuinely streamed).
- **A same-day backfill of a PAST day never inherits THIS boot's
  restarts (round 4):** the boot-reconciled process-death rows are
  day-filtered to the run's target day before they fold, so a mid-market
  restart used to launch a past-day backfill can no longer stamp the past
  day Partial with today's restart.
- **Re-run today:** restart with `TICKVAULT_SCOREBOARD_NOW=1`. Idempotent —
  the daily row carries the deterministic 15:45 IST ts and the episode rows
  reuse their audit-row ts, so everything UPSERTs in place. UNSET the
  variable afterwards: left set in a service unit, every 08:31 boot turns
  into an early partial run that consumes the day's single scheduled run.
  On a NON-trading day (weekend/holiday), `TICKVAULT_SCOREBOARD_NOW=1`
  alone is REFUSED with the "scorecard did NOT run" page — without the
  refusal it targeted the non-trading TODAY and (after 15:45) fabricated
  two all-zero rows stamped `complete`. A weekend re-run of a PAST day
  therefore REQUIRES the DATE var (below).
- **⚠ Re-runs are KEY-idempotent, NOT value-idempotent.** Every re-run
  RE-CLASSIFIES the day's episodes with the evidence available AT THE
  RE-RUN INSTANT, and the tables are last-write-wins on the DEDUP key. A
  re-run targeting any day OLDER THAN YESTERDAY (the errors.jsonl evidence
  horizon: the 48h MTIME sweep guarantees full-day file coverage only for
  yesterday — a 2-day-old target's session-hour files are already swept
  for any run after ~10:00, so round 5 tightened the day-granular gate
  from 2 to 1 — including the SCOREBOARD-01 triage advice "re-run once
  QuestDB is healthy" followed late) would have OVERWRITTEN evidence-backed
  blame
  with evidence-less defaults (`ours/dual_instance` → `broker/
  rate_limit_805`, corroborated `broker/bare_rst` → `indeterminate`).
  The keep-better guard now suppresses exactly that: a partial-evidence
  run never replaces an existing `run_partial=false` row — the existing
  verdict is kept, folded into the day's tallies, and the suppression is
  logged loudly (`SCOREBOARD-01`, `stage="blame_regression"`). Same-day
  re-runs with intact evidence remain fully idempotent. Residual: if the
  keep-better read itself fails (`stage="keep_better_read"`), the guard
  is OFF for that run — prefer re-running a stale day only when the
  QuestDB read side is healthy.
- **Backfill a PAST day:** restart with `TICKVAULT_SCOREBOARD_NOW=1
  TICKVAULT_SCOREBOARD_DATE=YYYY-MM-DD` (both required — the date is only
  honored alongside the forced-run flag; unset both afterwards). Disconnect
  episodes + blame + per-feed coverage totals + exclusive/both minutes
  rebuild from the retained `ws_event_audit` + `ticks` tables; the run is
  DEDUP-idempotent. A malformed date REFUSES the run loudly (strict
  `YYYY-MM-DD`, fail-closed), and so do a NON-TRADING or FUTURE target date
  (they would fabricate an all-zero "complete" day) — you get the
  "scorecard did NOT run" page, never the wrong day's row. A backfill run
  skips the same-session audit-drop cross-check (that counter belongs to
  TODAY's session, not the target day).
- **Forced run BEFORE 3:45 PM:** allowed (dry-run), but the row is stamped
  `partial_coverage=true` / `outcome=partial` and the Telegram says
  "produced early on operator request" — a mid-day card never masquerades
  as the end-of-day record. The once-per-process task has then consumed the
  day's scheduled run; re-run after close (or let a post-close restart's
  catch-up run) for the complete row.
- **What a re-run/backfill CANNOT recover:**
  - errors.jsonl correlation older than the 48h retention → 805 episodes
    default `broker`, resets default `indeterminate` (rows flagged
    `run_partial`).
  - Lag histograms for a past day (they are in-memory, per-process,
    day-scoped — reset at IST midnight) → a re-run measures nothing
    itself, but the lag keep-better (review round 1, 2026-07-11) folds
    the day's EXISTING measured lag columns forward, so a rerun/backfill
    can never erase a measured distribution with −1; sentinels stay only
    when NOTHING ever measured the day. A mid-day restart loses the
    pre-restart window: a pre-close 15:45 run carries the post-restart
    distribution, stamped partial with the restart footnote
    (measured-but-partial, never fabricated); a POST-CLOSE restart's
    catch-up rerun has ZERO post-restart samples — its row keeps the
    15:45 run's measured columns via the same keep-better
    (`stage="lag_regression"` logs the suppression).
  - Per-instrument unique-wins for pre-PR-D days (before 2026-07-11) AND
    for any past-day backfill: the presence registry is process-local +
    day-scoped, so only the day's own same-day 15:45 run (or a same-day
    forced rerun) can read it. The coverage keep-better
    (`stage="coverage_regression"`) folds a measured day's
    registry-derived columns forward, so a backfill can never erase them
    with the SQL approximation; `coverage_source` records which source
    each row's numbers came from (`in_memory` full-session registry /
    `mixed` post-restart partial registry / `sql_backfill`). Residual
    (equal rank is value-aware only at ZERO — PR-D review round 2): a
    FORCED mid-day run's coverage row (`TICKVAULT_SCOREBOARD_NOW` at,
    say, 14:00 → `mixed`) can be replaced by a later same-day run's
    SMALLER nonzero post-restart window (forced run → in-session crash →
    pre-close reboot; `mixed` == `mixed` with a nonzero drain wins) —
    annotate such days rather than trusting the smaller window; the
    normal 15:45-first-write flow cannot hit this shape.
  - Stall episode rows for days before the stall event kind shipped
    (PR-B, 2026-07-10) — those days honestly read `stalls = 0` in the
    table and on the card (0 = "no rows existed", not "no stalls
    happened"); the CloudWatch counter
    `tv_feed_sidecar_stall_restart_total` holds the pre-ship past. From
    PR-B forward the `stall_restarted` ws_event_audit rows are durable,
    so a backfill of a POST-ship day rebuilds its stall episodes fully.
  - A process death whose boot's connect row NEVER landed inside the
    reconcile poll window (below) — the coverage hole still shows in
    `streaming_minutes`, but the episode row is absent.
  - **Boot-reconciled process-death rows in general:** only the BOOT that
    detected the death can synthesize them (the pairing needs the last
    pre-boot "up" row against THIS boot's first connect). If that boot's
    write never landed — QuestDB down through all 3 flush retries, or the
    reconciler crashed — the rows are gone for good; a
    `TICKVAULT_SCOREBOARD_NOW` re-run re-aggregates the day but can NOT
    re-create them, and the month restart count under-counts (the
    SCOREBOARD-01 `reconcile_flush` / `reconcile_panic` error lines are the
    evidence trail).
- **Process deaths** are reconciled once per boot on BOTH boot paths
  (normal start AND the fast crash-recovery restart; the boot anchor is
  the PROCESS-START instant, so connections made early in the fast boot
  still count as this boot's own): the first query fires ≈3 min after
  start, then POLLS every 60s (up to ~12 min total) PER KEY until every
  connection that was up before the boot shows its own post-boot
  connect/reconnect — covering a restart that waits out the 5-minute
  rate-limit cooldown before reconnecting even while the other feed
  reconnects in seconds. A death is synthesized when the DEATH WINDOW —
  from the last pre-boot "up" record to this boot's reconnect — overlaps
  the session (so the normal day's pre-market ~08:34 connect followed by
  an 11:00 crash counts and a purely pre-market crash does not) — the
  16:30 auto-stop → NEXT-day 08:30 start cycle never counts as a death
  (the prior day's rows are day-scoped out), and a SAME-day reconnect
  landing after the 3:30 PM close (the 16:30 stop → manual evening start
  workflow, or a just-post-close crash) is recorded as a forensic
  `post_close_restart` row that is EXCLUDED from the headline
  restarts/blame counts and the partial floor — with an edge-triggered
  audit it is indistinguishable from the clean scheduled stop, so it must
  never re-write a completed day's card. Failed episode flushes retry 3×
  at 60s spacing before giving up loudly (`stage=
  "reconcile_flush_exhausted"` — the counter and the success line then do
  NOT fire; the boot's own card still counts the deaths in-memory, but
  the rows never reached the table and the month restart count
  under-counts).

## 5. Day-1 notes (PR-A scope)

- Disconnect episodes + blame + per-feed coverage totals + feed-level
  exclusive/both minutes work from day 1 (they aggregate the EXISTING
  `ws_event_audit` + `ticks` tables — any past day is backfillable for
  those via `TICKVAULT_SCOREBOARD_NOW=1 TICKVAULT_SCOREBOARD_DATE=…`, §4).
- Lag is MEASURED since PR-C (2026-07-11): the per-feed day histograms
  feed the lag columns + the card's delay lines from the ship date
  forward (pre-ship days + backfills stay `-1` and render "not measured
  yet"; the card footnote carries the Dhan ≥1s floor + the Groww
  millisecond/sidecar-capture semantics). Stalls are MEASURED since
  PR-B (2026-07-10): the stall
  watchdog's `stall_restarted` rows feed the Stalls column, 0 = measured
  0 from the ship date forward, and the PR-1 "?" footnote is retired
  (pre-ship days: see the §2 floor caveat + §4 backfill note).
  Per-instrument coverage is MEASURED since PR-D (2026-07-11): the
  presence registry pairs instruments across feeds (ISIN for stocks,
  canonical index name for indices, contract identity for the §36
  futures) and the card's "Minutes only I had" line is registry truth on
  `in_memory` (full-session) days ONLY — on `mixed` days it carries the
  SQL minute-set values (fix round 1). Unmapped singletons are counted
  (`unmapped_instruments`) + named in the day's logs (bounded sample) —
  never silently dropped. Per-instrument detail stays table-only
  (`feed_coverage_daily`), never rendered on the card.
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
- [ ] Stall counts note the PR-B ship date (2026-07-10 — earlier days
      read 0 = "no rows", not "no stalls") AND that the stalls column is
      Groww-only by construction (the Dhan main-feed's silent-socket
      recoveries live in its disconnect/reconnect rows) — compare
      incident totals, never the stalls column across feeds.
- [ ] `coverage_source='mixed'` days: the feed-level `unique_win_minutes`
      / `both_minutes` are the SQL minute-set values (PR-D fix round 1 —
      the partial-session registry window is cross-feed ASYMMETRIC on
      restart days, so only a full-session registry flips those two
      columns); the per-instrument mapped/unmapped/covered columns are
      the registry's PARTIAL measurement on such a day. Residual (dev/
      manual midnight-crossing runs): a post-09:15 FIRST registration
      also stamps `mixed` — the destroyed pre-registration minutes are
      not recovered, only honestly labelled.
