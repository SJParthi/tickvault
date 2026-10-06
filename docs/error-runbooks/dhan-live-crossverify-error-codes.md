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
