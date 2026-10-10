---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/app/src/dhan_live_crossverify.rs"
  - "crates/storage/src/dhan_live_crossverify_persistence.rs"
---

# Dhan Live-vs-REST Cross-Verification — Error Codes (DHAN-LIVE-XVERIFY-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `no-rest-except-live-feed-2026-06-27.md` §8 (the REST tape this compares
> against) > this file.
> **Companion code:** `crates/app/src/dhan_live_crossverify.rs` (the supervised
> daily runner + `/exec` readers + pure `compare_day`) and
> `crates/storage/src/dhan_live_crossverify_persistence.rs` (the audit tables).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention `DHAN-LIVE-XVERIFY-01` and
> `DhanLiveXverify01RunDegraded` verbatim — both appear below.

---

## §0. Why this comparator matters more than any other audit surface

The Dhan India live feed carries **no sequence number** and offers **no
snapshot-on-subscribe**, so packet loss is undetectable at the protocol level.
Comparing our aggregated `candles_1m` (`feed='dhan'`) against Dhan's OWN
official 1-minute tape (`/v2/charts/intraday`) is therefore the only ground
truth available, and a non-zero `compared` from it is the only evidence this
repository can offer that the live feed works at all.

Its predecessor was **BLIND SINCE BIRTH**: it compared NANOSECOND literals
against QuestDB's MICROSECOND `TIMESTAMP` column, so its `WHERE` window sat
around year 58502 and matched ZERO rows on every run it ever made. It honestly
reported `compared=0` — and therefore never fired a mismatch page, for the
entire life of the feature (fixed by PR #1474, commit `f84b4398`).

**This history is the reason `Blind` is a first-class outcome here rather than
a variant of success.** A run that compared nothing must never render green.

---

## §1. DHAN-LIVE-XVERIFY-01 — run degraded

`ErrorCode::DhanLiveXverify01RunDegraded`
(`code_str() == "DHAN-LIVE-XVERIFY-01"`).
**Severity:** High. **Auto-triage safe:** YES — the comparator is read-only
over `candles_1m` and the REST tape, and writes only its own audit tables.
**Delivery:** log-sink-only.

**Trigger:** a leg of the daily comparison could not complete — an HTTP client
build failure, an `/exec` query failure or truncation, an audit-flush failure,
or a run that exceeded its wall-clock budget. The verdict is reported as
`Degraded` or `Blind`, never as a pass.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `DHAN-LIVE-XVERIFY-01`; the
   payload names the `comparator`, the `stage`, and the underlying error.
2. A `Blind` verdict means the comparison ran but matched no rows. Check the
   day's `candles_1m` row count for `feed='dhan'` before suspecting the
   comparator: an empty live table is a FEED outage, not a verifier bug.
3. A truncation stage means the row cap bounded the read. The counts stay
   exact; only the per-row detail was cut.
4. An audit-flush failure discards pending rows by design (poisoned-buffer
   defense). The rerun is DEDUP-idempotent.

**Honest envelope:** the comparator can only catch us disagreeing with Dhan.
It cannot catch Dhan being wrong, and it cannot see loss that happens upstream
at the vendor's side — Dhan's published architecture skips a slow consumer
forward to "the latest available state" with no sequence number, so ticks
discarded there are invisible to every counter we own.

---

## §2. 2026-10-06 — the day marker: failure reasons and sources (plan ITEM 51a)

Authority: `no-rest-except-live-feed-2026-06-27.md` §12.15.7 and the noise
lock §2.5 note of the same date. Every line below is emitted with
`code = "WS-GAP-03"` (`ErrorCode::WsGapConnectionState`) from
`crates/app/src/dhan_live_crossverify_boot.rs`, except `xverify_marker_keep_short`,
which `crates/app/src/main.rs` emits once at boot. Only `xverify_failed` is
alarmed; the other sources are log-sink-only.

| Line | Level | Means | Operator action |
|---|---|---|---|
| `source = "xverify_failed"`, `reason = "marker_not_written"` | ERROR (pages, last attempt only) | the comparison finished and every audit row was accepted, but the day marker file could not be saved on any attempt; today's S3 archive stays held | free disk space, then check permissions on `data/state/daily`; a restart re-runs the check and writes the marker |
| `source = "xverify_failed"`, `reason = "audit_rows_lost"` | ERROR (pages, last attempt only) | on every attempt some cell, tape or daily rows were discarded by a failed flush or refused at append, so the day was not recorded | check QuestDB health and disk first (`make doctor`); the `xverify_persist_partial` / `xverify_persist_failed` lines carry `rows_discarded`; a restart re-runs the check (DEDUP-idempotent) |
| `source = "xverify_attempt_marker_write_failed"` | WARN (per attempt) | this attempt could not save the marker; the next attempt only writes the marker (no new vendor fetch) | free disk space, then check permissions on `data/state/daily`; the `path` field names the file |
| `source = "xverify_marker_dir_sync_failed"` | WARN | the marker was saved and renamed into place, but the folder sync failed; the day IS recorded, a host crash could lose it | free disk space, then check permissions on `data/state/daily`; no re-run is needed |
| `source = "xverify_marker_keep_short"` | WARN (once at boot) | a configured archive hot window plus the 3-day hold comes within 3 days of the 400-day marker keep, so a verified day could read as unverified | lower the hot window in `[partition_retention]` or raise `CROSSVERIFY_MARKER_KEEP_DAYS`; free disk space, then check permissions on `data/state/daily` if markers are missing |

**Honest limit:** markers deleted by the old 7-day sweep before this change
cannot be recovered; those days take the hold-ceiling override once their
partitions age past the hot window.

## §3. 2026-10-06 — attempt time bounds: sources (plan ITEM 51b)

Authority: `no-rest-except-live-feed-2026-06-27.md` §12.15.8. Every line
below is emitted with `code = "WS-GAP-03"` (`ErrorCode::WsGapConnectionState`)
from `crates/app/src/dhan_live_crossverify_boot.rs`. Every row is
log-sink-only (no alarm, no filter, no page) except the one
`xverify_failed` row, which is the existing page. When the attempt a line
describes was the day's last, the existing `xverify_failed` page fires after
it with `reason = "incomplete"` (`"not_persisted"` after a persist stopped at
the deadline), unless today's paged marker shows the day was already paged
(then `xverify_already_paged_today`). A persist stopped at its deadline counts
only on the local-only `tv_dhan_xverify_persist_deadline_stops_total{pass}`
and `tv_dhan_live_xverify_audit_rows_abandoned_total`, never on the
`audit-rows-lost` page's counters: a failed flush still pages there, a
deliberate stop does not.

| Line | Level | Means | Operator action |
|---|---|---|---|
| `source = "xverify_attempt_timed_out"` | WARN (per attempt) | the attempt (token wait + run) did not finish within its limit (`limit_secs`) and was stopped at its last wait, or (`stage = "marker"`) it reached the limit before writing the marker; either way it ends before the 17:25 scheduled stop; this attempt does not record today | check how long the token took and whether QuestDB or the vendor was slow (`make doctor`); the next attempt, if any, runs with the budget the time left allows (shrunk late in the day, never ending after 17:23 IST) |
| `source = "xverify_attempt_skipped_no_time"` | WARN (per attempt) | the attempt started too late to run at least 120 s of comparison and still end by 17:23 IST, so it was not started (`start_ist_secs`); typically a restart between about 17:15 and 17:50. When nothing ran before it in this process, the day is reported at once (next rows) and one attempt is made at 17:50 if the box is still up then. On a weekday a box launched before 17:30 is not: the start watchdog stops it at 17:45 even with keep-alive set (and the hourly curfew at 17:35 without it) | none for the attempt itself; if the day stayed unverified, a process started at or after 17:50 IST the same day runs one full check at boot and can record the day. On a weekday, let the 17:45 stop happen and start the box by hand after it: a box started after 17:30 is not stopped by the 17:45 check |
| `source = "xverify_failed"`, `reason = "skipped_no_time"` | ERROR (pages, once a day at most) | the day's only attempt in this process was skipped for time and no page went out today (no paged marker), so nobody had been told today is unverified; today's S3 archive stays held. One attempt follows at 17:50 if the box is still up then (on a weekday only a box started by hand after 17:30 is; see the row above) | if the box is still up at 17:50, wait for that attempt (its result is logged; a failure does not page again); on a weekday, start the box by hand after 17:45: its process checks today at boot when it starts at or after 17:50, or waits for 17:50 when it starts before. If no process runs at or after 17:50 today, the S3 hold ceiling applies (the next day's run does not re-check today). A later `xverify_vacuous` the same day does not page again (see the vacuous row note in §12.15.8); its log line carries `reason = "vacuous"` |
| `source = "xverify_day_not_attempted"` | WARN (once a day at most) | as above, but today's paged marker (`path`) shows a page already went out today, so this process does not page again; today's S3 archive stays held | nothing more for the page; if the box is still up at 17:50 one more attempt runs (on a weekday only a box started by hand after 17:30) |
| `source = "xverify_already_paged_today"` | WARN (once a day at most) | a final failure (`reason`) after today's page already went out (paged marker `path`); not paged again | the earlier page's action applies; the S3 hold stays until a later run records the day or the hold ceiling |
| `source = "xverify_paged_marker_write_failed"` | WARN | a page went out but the paged marker could not be saved, so a later failure the same day may page again | free disk space, then check permissions on `data/state/daily` |
| `source = "xverify_options_timed_out"` | WARN (once a day at most) | the depth-held option pass did not finish within its limit and was stopped; outcome label `timed_out` on `tv_dhan_xverify_option_pass_total` | none required (the option pass never holds S3 or pages); if it repeats, check vendor latency |
| `source = "xverify_persist_stopped_at_deadline"` | WARN (per attempt) | the audit persist stopped because its next flush, in the database client's worst case (5 s + the buffer at 100 KiB/s), could not finish by the attempt's limit; buffered rows were abandoned (`rows_abandoned`), the rest never written (`rows_not_written`); the attempt fails `not_persisted`, no marker | check QuestDB write latency and apply lag (`make doctor`); the comparison is recomputable, so the next attempt or the next day re-writes it (DEDUP-idempotent) |
| `source = "xverify_options_persist_stopped_at_deadline"` | WARN (once a day at most) | the option pass's audit persist stopped the same way before its own limit (`abandoned`, `not_written`) | none required (the option pass never holds S3 or pages); if it repeats, check QuestDB write latency |

**Honest limit:** the timeout can stop an attempt only while it waits (token,
database read, vendor fetch). The audit write runs after the comparison
returns, so it bounds itself: it never starts a flush whose worst case ends
after the limit. That the database client gives up at its request timeout
plus the bytes at its minimum throughput is read from its source; that the
HTTP timeout covers the whole request is assumed. The marker write is one
small file and is not bounded on a stalled disk.

## §4. 2026-10-06 — the verdict and the late window: columns and cell kinds (plan ITEM 51c)

Authority: `no-rest-except-live-feed-2026-06-27.md` §12.15.9. No new log
source, alarm, filter or page: the existing `finished` line of
`crates/app/src/dhan_live_crossverify_boot.rs` carries the new counts, and
`xverify_diverged` stays price-only.

| Column / field / kind | Where | Means | Operator action |
|---|---|---|---|
| `late_excused` (LONG), cell kind `late_excused` | `dhan_live_crossverify_daily`, `dhan_live_crossverify_cell_audit`, `finished` line | a traded or index minute missing from our side inside the end-of-session window (`LATE_SEAL_WINDOW_MINUTES`, 15:35 to 15:39 today), after that instrument's last live minute, that may not have sealed by the read (a gap before a later live bar of the same instrument is judged, never excused); never real, holds the day at `partial` | none for one day; if it is non-zero most days, check the seal catch-up (plan item 51d judges these minutes once the read waits for the seal) |
| `missing_live_unjudged` (LONG), cell kind `missing_live_unjudged` | same | the live read hit its row cap, so a missing traded or index minute could not be told from an unread one; never real, holds the day at `partial` | check that `live_truncated = true` on the `finished` line (and `missing_judgeable = live_truncated` on the daily row); a cap hit means the day's live rows outgrew the read cap |
| `missing_judgeable` (SYMBOL: `judged` / `live_truncated`) | `dhan_live_crossverify_daily`, `finished` line | whether missing minutes were judged on this run | `live_truncated`: as the row above |
| `missing_live_late` | `finished` line only | traded or index minutes missing in the late window after that instrument's own last live minute, whatever the policy; a measurement for plan item 51d | none |
| `tail_unsealed` column and kind | both tables | the old literal two-minute tail; written 0 from 2026-10-06 and kept for older rows | none |
| `attempt_at` (TIMESTAMP), `run_complete` (BOOLEAN) | `dhan_live_crossverify_daily` (both), `dhan_live_crossverify_cell_audit` (`attempt_at`) | `attempt_at`: when the attempt that wrote the row persisted it, one reading per attempt; `run_complete`: at most 5% of that attempt's vendor fetches failed, the budget was not spent and the live read was not truncated | pick the day's final row and its cells with them (below) |

**Behaviour change:** a day with a price difference or a judged missing traded
or index minute now reads `diverged` even when a vendor fetch failed or the
budget ran out (it read `partial`).

**A day can have more than one daily row, and the same day's retry is the
usual cause.** Every attempt writes its daily row at the same `ts`, and the
daily DEDUP key includes `outcome`. An attempt that is not complete (more than
5% of its vendor fetches failed, the budget ran out, or the live read was
truncated) persists its rows and is retried; if the retry reads differently
(for example `diverged` at 15:41 because one sealed bar was still waiting in
QuestDB's WAL, then `partial` at 15:54 once it was readable) both rows stay.
Cells the first attempt wrote and the retry did not (that missing minute) stay
too. **A later attempt never outranks a `diverged` one** whose rows reached
the database (§12.15.9, corrected
2026-10-10): a retry can read lower for a bad reason, because a target whose
vendor fetch fails on the retry adds only `missing_rest` and its divergence is
simply not judged again. The day reads `diverged` when ANY daily row of the
day reads `diverged` (query 1) OR query 3 finds any real spot finding; only
when both come back empty is the verdict the newest row (query 2). Query 3 is
part of the rule because an attempt's persist can stop after some of its cells
were flushed and before its daily row (the §12.15.8 deadline stop, or a failed
batch flush): those cells are real findings that no daily row carries
(round-4 review 2026-10-10).

```sql
-- 1. The day's verdict when any attempt read diverged (newest such attempt).
SELECT * FROM dhan_live_crossverify_daily
WHERE trading_date_ist = '<day>T00:00:00.000000Z' AND outcome = 'diverged'
ORDER BY attempt_at DESC LIMIT 1;

-- 2. Only when query 1 returns nothing (and query 3 returns nothing): the
--    newest attempt.
SELECT * FROM dhan_live_crossverify_daily
WHERE trading_date_ist = '<day>T00:00:00.000000Z'
ORDER BY attempt_at DESC LIMIT 1;

-- 3. The day's spot findings, whatever attempt wrote them (newest write
--    first). The option pass (segment NSE_FNO) is left out: it writes no
--    daily row and is not part of the day's verdict.
SELECT * FROM dhan_live_crossverify_cell_audit
WHERE trading_date_ist = '<day>T00:00:00.000000Z'
  AND segment <> 'NSE_FNO'
  AND (kind = 'diverged'
       OR (kind = 'missing_live' AND (rest_volume > 0 OR segment = 'IDX_I')))
ORDER BY attempt_at DESC;

-- 4. The §12.15.6 option pass's findings (never part of the day's verdict).
SELECT * FROM dhan_live_crossverify_cell_audit
WHERE trading_date_ist = '<day>T00:00:00.000000Z'
  AND segment = 'NSE_FNO'
  AND (kind = 'diverged' OR (kind = 'missing_live' AND rest_volume > 0))
ORDER BY attempt_at DESC;
```

Query 3 leaves out a `missing_live` cell whose Dhan minute printed volume 0 on
a non-index instrument: such a minute is never a real finding (§12.15.9). It is
still written with kind `missing_live` and counted in the daily row's
`missing_live_zero_volume`.

When query 1 finds a `diverged` row and a newer row of the day reads
`partial` or `clean`, read the newer row too: until plan item 51d, a sealed bar
not yet readable at the first read makes a `diverged` that the retry no longer
sees, and its missing cells name the minutes involved. A divergence in prices
(`cells_diverged > 0`) does not come from an unreadable bar.

`run_complete = false` on the chosen row means that attempt failed more than
5% of its vendor fetches or was cut short, and no day marker was written for
it. A row with a null `attempt_at` was written before 2026-10-06; for such a
day, read every row. Do not pick a day's findings by matching `attempt_at`:
neither key carries the attempt, so an attempt that finds a cell again
overwrites it with its own stamp, two attempts with the same `outcome` share
one daily row with the later stamp and counts, and a finding no later attempt
re-found keeps an earlier stamp that may match no surviving daily row. A cell's
`attempt_at` is the last attempt that wrote it. The option pass writes cells
with its own `attempt_at` and no daily row, so its cells are read with query 4
and never change the day's verdict.

Query 2 picks the newest row whatever it reads, so a vacuous retry
(`degraded`, `blind`, `no_data`) can shadow an earlier measured `partial` or
`clean` row. Neither writes a marker for the vacuous attempt; read the earlier
rows when query 2 returns a vacuous outcome. A day whose live side lost every
minute reads `blind` (nothing lined up) or `degraded` (a fetch also failed or
the read was cut short), not `diverged`, even when the vendor tape has traded
minutes: its cells are real `missing_live` findings, so query 3 makes the day
`diverged`, unless the live read was truncated (its cells are then
`missing_live_unjudged`, which query 3 does not count).

The rule only sees rows that reached the database. An attempt whose persist
stopped before its first flush (the §12.15.8 deadline stop) leaves no daily
row and no cell, only the `finished` line and the
`xverify_persist_stopped_at_deadline` warning. If that attempt found a
divergence and a later attempt could not re-judge it (that target's fetch
failed), the day reads `partial`. Search the app log for that warning on a
day that reads `partial` (round-5 review 2026-10-10). The queries are Assumed: they were not run against a live QuestDB
when written.

**A `diverged` day is not always packet loss (until plan item 51d).** A
traded or index minute is also missing from our side when its sealed bar was
not readable at the read (still queued for the seal writer, staged in a spill
file not yet replayed, or not yet applied by QuestDB's WAL) or when the read
skipped its row as malformed. Before treating a `diverged` day as lost ticks,
check `malformed_rows` on the `finished` line, the WAL apply lag at the read
and the spill directory for that day, then re-run the day once QuestDB has
caught up.

## §5. 2026-10-06 — the read waits until our candles are sealed and saved (plan ITEM 51d)

Authority: `no-rest-except-live-feed-2026-06-27.md` §12.15.10. No new alarm,
filter, page or EMF name. Since 51d the paragraph above ("A `diverged` day is
not always packet loss") applies only to a live row skipped as malformed and
to a seal sent to the dead-letter queue: an attempt no longer reads while a
sealed bar is queued, staged in a spill file or not yet applied by QuestDB.

| Source / reason / value | Level | Means | Operator action |
|---|---|---|---|
| `source = "xverify_attempt_not_ready"`, `reason = "seals_pending"` | WARN (log only) | the seal writer had not drained after the last catch-up sweep, or a seal spill file for today was still staged; nothing was compared and the check runs again later today | none for one attempt; if every attempt says it, list the spill folder (below) and check the seal writer is running |
| same, `reason = "seal_spill_parked"` | WARN (log only) | the mid-session replay parked a spill file after repeated failed flushes | check QuestDB is accepting writes, then the spill folder; the parked file replays once QuestDB answers |
| same, `reason = "live_not_applied"` | WARN (log only) | QuestDB had not applied `candles_1m` through the snapshot taken after the drain, or `wal_tables()` could not be read | run the `wal_tables()` query (below); a `writerTxn` well behind `sequencerTxn` is apply lag |
| same, `reason = "live_not_final"` | WARN (log only) | the catch-up floor was still moving below the close, or a strict read with no seal progress found end-of-session minutes missing (only Dhan's record was saved) | none for one attempt; on every attempt, check that the feed's frames reached the close |
| `source = "xverify_unsettled_final"` | WARN (log only) | the day's LAST attempt read with our candles not known final: prices were compared, missing minutes were not judged | read the day's `missing_judgeable` (below); the day is `partial` at best |
| `missing_judgeable` = `not_ready_seals_pending` / `not_ready_seal_spill_parked` / `not_ready_not_applied` / `not_ready_completeness_unknown` | daily row, `finished` line | the last attempt's reason for not judging missing minutes | as the matching row above |
| `tv_dhan_xverify_option_pass_total{outcome="skipped_not_ready"}` | `info!` | the option pass did not run because the spot check found our candles not saved | none |
| `xverify_failed`, `reason` = one of the four above | ERROR (pages, last attempt only) | every attempt found our candles not ready and the last one could not read either | as the matching row above; S3 stays held |

```sql
-- QuestDB WAL apply state for the table the check reads.
SELECT name, suspended, writerTxn, sequencerTxn
FROM wal_tables()
WHERE name = 'candles_1m';
```

```bash
# Seal spill files still waiting for today (top level and replaying/).
ls -l /opt/tickvault/data/spill /opt/tickvault/data/spill/replaying | grep "seals_v4-$(TZ=Asia/Kolkata date +%F)"
```

**Honest limits.** A seal floor frozen below the close excuses the minutes
after it (the day reads `partial`). The spill check counts every staged seal
file of the day, not only 1-minute bars. A seal sent to the dead-letter queue
is not checked and still reads as a judged missing minute. The `measured`
runs counter counts an attempt that kept only Dhan's record and retried. The
SQL and the `ls` line are Assumed: they were not run against the live box when
written.
