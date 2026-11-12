---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/app/src/live_fill_report/**"
  - "crates/storage/src/live_fill_report_persistence.rs"
---

# Live Paper Fill Report Error Codes

> **Authority:** Quote 57 in
> `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`
> (owner, 2026-11-12) and plan item LF-7
> (`.claude/plans/active-plan-audit-2026-10-20.md`).
> `ErrorCode::LiveFill01ReportFailed.runbook_path()` and
> `ErrorCode::LiveFill02FillOutsideWorstCase.runbook_path()` point here.
> **Neither code pages.** Both are log-only (`LOG_ONLY_CODES` in
> `crates/common/tests/error_code_paging_decision_guard.rs`); the noise lock
> allows no new page without a dated row first. PAPER ONLY: the report reads
> and writes files; it never places, changes or cancels an order.

## What the report is

Once per trading day (attempts at 15:47, 15:54 and 16:01 IST, never inside
09:00–15:40; a start after 16:01 runs one final attempt at once when it fits
before the 16:25 stop window), the app reads the day's `live_order_events`, re-prices every
paper order from the stored `candles_1s` with the chain's own pricing (the
LIMIT volume cut, the worst-case model on the best quote, the LIMIT share cut,
the whole-lot floor), and writes five files to `data/live-fill-logs/<D>/`:
`report-<D>.json`, `orders-<D>.csv`, `fill-seconds-<D>.csv`,
`latency-<D>.csv`, `rejects-<D>.csv`. They are copied to
`s3://tv-<env>-cold/live-fill-logs/<D>/` (never overwritten, never deleted),
one summary row goes to `live_fill_report_daily`, and the day's marker
(`data/state/daily`, task `live_fill_report`) is written only when all five
copies are verified. The row is written once the files are written and again
after a verified copy; its designated `ts` is the day, so the later write
replaces the earlier one (one row per day). A boot picks up unfinished days, up
to three trading days back, only after today's attempts and only while a whole
run stays outside 09:00–15:40 and clear of the stop window. Day folders older
than those three trading days that were never copied (all five files have no
record in `uploaded/`) are then COPIED as they are, never rebuilt, at most
three per pass; a folder missing a file is reported once per process as
`stage="outside_lookback_incomplete"` and left in place. When a run is still
owed after 15:40 and the clock is inside 16:17–16:45 (no whole attempt fits
before or inside the stop window), the task wakes again at 16:45 the same day
instead of the next day, so a box held up by the drain hold still runs it.

A day with no orders reads Amber (`status = partial`, reason
`live_fill_not_running`) unless live fill was running in the process that wrote
the report (an approved strategy, or idle with none configured): an empty day
from a refused or never-started live fill is never Green.

Quick look in the QuestDB console:

```sql
SELECT * FROM live_fill_report_daily ORDER BY ts DESC LIMIT 20;
SELECT event, reason, count() FROM live_order_events WHERE ts IN '2026-11-12' GROUP BY event, reason;
```

## LIVE-FILL-01 — a report step failed

**Severity:** Medium. **Pages:** no. **Field `stage`:**

| `stage` | Meaning | What happens next |
|---|---|---|
| `query` | the day's `live_order_events` read failed | no files this attempt; the next attempt (or the next boot) retries |
| `truncated` | the day held more than 50,000 rows | the report is built from the first 50,000 and reads Amber |
| `recheck` | some order second reads failed | those orders read `recheck_unavailable` (Amber) |
| `write` | a local file could not be written | no copy, no marker; retried |
| `no_bucket` | no cold bucket resolved in time | files kept; the next boot copies them |
| `upload` | three copy tries, 60 s apart, did not verify all five files | files kept; the next boot copies them |
| `marker` | the copies are verified but the day marker did not land, or a file's local copy record in `uploaded/` could not be written (most often a full disk). Reported only here: the report never touches the raw-frame uploader's marker counter, so it cannot fire the cold-backup page | the next attempt or the next boot repeats the (idempotent) copy |
| `rebuild_after_partial_copy` | a copy of the day already started (`uploaded/copy-started` exists) but one of the five files is missing; the day is NOT rebuilt, so the bucket never holds files from two builds | the copy runs with what is there and reads `missing`; restore the file from the bucket or leave the day Amber |
| `db_row` | the summary row was not written after three tries | the files and copies are unaffected |
| `db_row_not_ensured` | `live_fill_report_daily` could not be created or checked (one re-try before the row) | the row is NOT written, so QuestDB never creates the table itself in the wrong shape; the files and copies remain the record |
| `db_row_from_files` | a catch-up copied the files but could not read `report-<D>.json` back for its summary row | the copies are verified; the row keeps its earlier `s3_uploaded = false` |
| `copy_timeout` | the copy ran past what the attempt had left | files kept; the next attempt or the next boot copies them |
| `attempt_timeout` | an attempt ran past 420 s (every step is budgeted inside it, so this is a backstop) | the next attempt runs |
| `old_day_scan` | `data/live-fill-logs/` exists but could not be listed, so folders older than the three-trading-day lookback were not checked this pass | nothing is deleted; the next pass or boot lists it again |
| `outside_lookback_incomplete` | a day folder older than the lookback was never copied and is missing one of its five files (logged once per process per folder) | the folder is kept and NOT rebuilt; copy what is there by hand (below) or leave the day Amber |

**Action:** none is urgent. If `upload` or `no_bucket` repeats, the local
folder is the only copy: check the box's bucket access (`make doctor`). Within
three trading days the day's own catch-up retries the copy; after that, every
pass of every boot still copies the folder as it is (at most three old folders
a pass, oldest first, `upload` or `no_bucket` again if it still fails), until
all five files have a record in `uploaded/`. Manual remedy if the box can no
longer copy it: `aws s3 cp` each file of `data/live-fill-logs/<D>/` (gzipped,
as `<name>.gz`) to `s3://tv-<env>-cold/live-fill-logs/<D>/` from a machine with
access, never overwriting a key that already holds other bytes.

## LIVE-FILL-02 — a paper fill outside the re-priced worst case

**Severity:** High. **Pages:** no. One line per order, at most 50 a report,
then one total line. The fields name the order (`signal_id`, `security_id`,
`segment`, `symbol`, `side`, `qty`, `fill_qty`, `avg_paise`, `recheck_qty`,
`recheck_avg_paise`, `gap_ticks`) and the `verdict`:

| `verdict` | Meaning |
|---|---|
| `worse_than_worst` | the paper average is worse than the re-priced worst case (a buy above it, a sell below it) |
| `better_than_worst` | the paper average is better than the re-priced worst case: the chain was optimistic against the stored data |
| `qty_differs` | the paper fill and the re-pricing filled different quantities (an exit the runtime cut to the paper position held is compared on the cut quantity, the Sent row's, so the cut alone is never a finding) |
| `stored_worse` | a stored row says the fill was worse than the model (live mode only) |

**Read it before trusting the fill model.** The chain priced from its own 1 s
bars; the report re-prices from the stored `candles_1s`. A late tick the chain
never saw can widen a stored second, so a difference may be a late-tick
difference rather than a defect. Open the day's `orders-<D>.csv`, find the
order, and compare its seconds:

```sql
SELECT ts, high, low, volume FROM candles_1s
WHERE security_id = <id> AND segment = '<segment>' AND feed = 'dhan'
  AND ts >= '<decision second>' AND ts <= '<decision second + 61 s>' ORDER BY ts;
```

## Honest limits

- Paper mode compares the chain against the stored data, not against a real
  fill. A real worse-than-worst check exists only with live orders, which
  Quote 57 does not allow.
- A catch-up day (rebuilt after `candles_1s` was archived) reads
  `recheck_unavailable` and Amber.
- Only the best quote is used, the same as the chain; depth levels are not read.
- The re-pricing cuts the seconds at the one after the trigger second, as the
  chain normally does; a pass that released several seconds at once could have
  given the chain one more second.
- Today every report reads "no approved strategy is configured": there is no
  approved strategy crate yet.
