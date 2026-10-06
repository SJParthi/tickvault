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
`crates/app/src/dhan_live_crossverify_boot.rs`. Only `xverify_failed` is
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
