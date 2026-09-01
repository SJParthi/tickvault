# Implementation Plan: write amplification on the ticks path

**Status:** DRAFT
**Date:** 2026-09-01
**Approved by:** pending — operator approved "all these fixes"; this file records
why the write-path half is DELIBERATELY not shipped in that batch.

## Design

### The measured fact

`tick_persistence.rs` (the `MAX_RETAINED_FLUSH_SPANS` doc) records a live
measurement: `ticks` is `PARTITION BY HOUR` on exchange LAST-TRADE time, and
**10.0% of one day's 64.3M ticks carried a `ts` more than an hour behind
arrival** — legitimately, because for an illiquid strike the last trade
genuinely was hours ago. Its conclusion, verbatim: **"Commit width is the
amplifier."**

Session totals: ~4,744 GB written against ~190 GB of logical rows.

### What is ALREADY mitigated

`MAX_RETAINED_FLUSH_SPANS = 2` caps the widest commit to roughly three 500 ms
flush spans rather than the ~128 the byte ceiling alone would permit. That
landed 2026-08-25. So the obvious lever is spent.

### Three candidate fixes, and why two are REJECTED

**(A) Re-stamp `ts` from receipt time — REJECTED, would cause a silent
data-quality regression.**
`dhan_contract_universe.rs` and `depth_rebalance.rs` both run
`FROM ticks WHERE ts >= today_ist_micros ... LATEST ON ts PARTITION BY
security_id`. Under exchange time an instrument that has not traded today is
correctly ABSENT. Under receipt time every conflated snapshot re-enters that
window carrying a stale LTT price AS IF CURRENT — so `fit_atm_window` and the
top-mover ranking would fit against hours-old prices, all session, with no
alarm. `ts` is also DEDUP key column 1, so re-stamping rewrites dedup identity
for every historical row.

**(B) `PARTITION BY HOUR` -> `DAY` — REJECTED for now, not on this disk.**
QuestDB cannot alter partitioning in place; the DDL is
`CREATE TABLE IF NOT EXISTS`, so changing the string does nothing to the live
table. It needs `CREATE TABLE AS SELECT` + `RENAME` over a multi-billion-row
table, which needs free disk — and disk exhaustion is the problem being solved.
Revisit only once there is headroom.

**(C) Split fresh rows from stale rows into SEPARATE commits — the candidate.**
The amplifier is the SPREAD of `ts` inside one commit, not its wall-clock
width. A commit holding 90% current-hour rows plus 10% hours-old rows drags the
whole commit into an out-of-order merge. Routing rows into two buffers at
append time — one for the current partition, one for everything older — makes
the 90% a pure append and confines merge work to a small, latency-insensitive
batch.

Why this survives the objections that killed (A) and (B): `ts` semantics are
UNCHANGED (so every ATM/mover query behaves exactly as today), the partition
scheme is UNCHANGED (so no migration), and no row is dropped, reordered within
its partition, or re-keyed.

## Edge Cases

- A commit containing ONLY stale rows: behaves exactly as today.
- A row exactly on an hour boundary: must go to the older bucket, never split
  across both.
- Two buffers double the peak producer memory: must be counted against
  `MAX_DEPTH_PRODUCER_BUFFER_BYTES`-style bounds, not left unbounded.
- WAL replay writes historical rows: replay is the case where nearly every row
  is "stale", so the split must not degrade replay into one row per commit.
- The stale buffer must still respect `MAX_RETAINED_FLUSH_SPANS` semantics or
  it re-creates the unbounded-accumulation own-goal that constant exists to
  stop.

## Failure Modes

- If the real amplifier is page-granular allocation
  (`QDB_CAIRO_WRITER_DATA_APPEND_PAGE_SIZE = 16 MiB` x 24 candle tables x ~17
  columns) rather than O3 merges, this change buys nothing and adds a code
  path. UNMEASURED — this is the main risk.
- Two commits per flush doubles commit count; QuestDB commit overhead is
  non-zero and unmeasured here.
- `market_depth` is ~80% of the session burn and is ALREADY arrival-stamped, so
  it has no O3 problem and this change does not touch it. Expected saving is
  therefore bounded by the ticks share — roughly 7% of total writes, NOT the
  whole 25x.

## Test Plan

- Unit: rows straddling a partition boundary land in the correct buffer; a
  boundary row is never duplicated or dropped.
- Unit: an all-stale batch and an all-fresh batch both behave as today.
- Property: for any input set, union of the two buffers == input, with no
  duplicates (conservation).
- DHAT: no allocation added to the per-tick append path.
- The decisive test is not a unit test: one post-close session with the flag on,
  comparing `VolumeWriteBytes`, WAL apply lag, and flush-failure counts against
  the previous session.

## Rollback

Single config flag, default OFF. Flip off + restart. No schema change, no
migration, no data touched, so rollback is complete and instant.

## Observability

- `tv_ticks_commit_split_total{bucket="fresh"|"stale"}` — proves the split is
  actually happening and gives the real stale fraction per session, which is
  currently only known from one day's measurement.
- Reuse existing `VolumeWriteBytes`, WAL apply lag, and the flush-failure
  counters for the before/after.
- No new CloudWatch alarm (the budget is at its cap); charted only.

## Why this is NOT in today's batch

It changes the write path for every tick, cannot be tested against a real
QuestDB from this container, and its central assumption (that O3 merges rather
than page allocation are the amplifier) is UNMEASURED. Shipping it blind on the
same day as three other changes would make an unexplained result
un-attributable. Sequence: land the three safe fixes, measure, then this.

## Cheaper lever to measure FIRST

`QDB_CAIRO_O3_MAX_LAG` is set to `60000000` (60s) against QuestDB's 600s
default, with the comment "Tighter out-of-order merge window." A tighter lag
means rows are held in memory for less time before commit, so FEWER
out-of-order arrivals are absorbed in memory and MORE force a merge. Raising it
back toward the default is one line, instantly reversible, and needs no code.
It should be measured before any code change, because if it moves the number
materially then (C) may not be needed at all.

UNVERIFIED: the exact distribution of staleness below one hour is unknown. Only
the >1h tail (10.0%) was measured. If most of the remainder sits inside 10
minutes, the o3 lag change alone could capture most of the benefit.
