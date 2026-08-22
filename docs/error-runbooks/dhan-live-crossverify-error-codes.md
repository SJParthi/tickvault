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
