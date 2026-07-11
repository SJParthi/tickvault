# Implementation Plan: index_constituency cross-day duplicate fix (pin designated ts → epoch 0)

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** operator standing directive 2026-06-29 (eliminate cross-day duplicates, make merge-ready)
**Changed crates:** storage, app, core

---

## Problem (CONFIRMED)

`index_constituency` accumulates a fresh ~742-row set per trading day. Its DEDUP
UPSERT key (`ts, index_name, security_id, exchange_segment, feed`) includes the
designated timestamp `ts`, which the shared builder stamps with the day-floored
IST trading-date midnight. Two trading days = two distinct `ts` values = two
distinct row-sets that never collapse. Operator's QuestDB
`GROUP BY ts` proof: June28 = 742 + June29 = 742 = **1484 rows**.

The mirror table `instrument_lifecycle` does NOT exhibit this: it pins its
designated `ts` to a constant epoch 0 (`lifecycle_designated_ts_nanos() -> 0`),
so its DEDUP fires on the business key alone and it never duplicates across days.

This is the same I-P1-08 bug class already fixed for the lifecycle/snapshot
tables (see `gap-enforcement.md` I-P1-08). `index_constituency` was missed.

No date business column exists on the `index_constituency` row, and no Rust code
reads `index_constituency` rows back by `ts` (the in-memory `IndexConstituencyMap`
is built from the niftyindices/Dhan CSV, not from the table). The table is a
current-state map-only master — exactly like `instrument_lifecycle` — so pinning
`ts` to a constant and dropping per-day history is correct and intentional.

---

## Design

Three coordinated changes, all in the `storage` crate plus one boot wire-up, plus
a one-time data migration for the existing 1484 rows.

### A. Pin the designated `ts` to constant epoch 0 (the root fix — both feeds)

The designated `ts` is stamped at the SHARED builder
`crates/storage/src/index_constituency_persistence.rs:256`:

```rust
buffer.at(TimestampNanos::new(row.trading_date_ist_nanos))?;   // line 256 — the bug
```

BOTH writers funnel through this one builder:
- Dhan: `crates/app/src/index_constituency_boot.rs:169` (sets `trading_date_ist_nanos`)
- Groww: `crates/core/src/feed/groww/shared_master_writer.rs:253` (sets `trading_date_ist_nanos`)

So fixing this ONE `.at()` call fixes BOTH feeds simultaneously.

Mirror the lifecycle pattern exactly. Add an analogous constant-ts helper next to
the `index_constituency` DEDUP-key constants (mirroring
`instrument_lifecycle_persistence.rs:152` `lifecycle_designated_ts_nanos() -> 0`):

```rust
/// The pinned constant designated timestamp (epoch 0) for the
/// UPSERT-in-place `index_constituency` current-state master table.
/// Mirrors `lifecycle_designated_ts_nanos()` — the I-P1-08 rationale:
/// QuestDB requires the designated `ts` in the DEDUP key, so we pin it
/// to 0 and let the business key (index_name, security_id, exchange_segment,
/// feed) do the deduplication. Prevents the cross-day duplicate accumulation
/// (742/day) the day-floored `trading_date_ist` caused.
#[must_use]
pub const fn index_constituency_designated_ts_nanos() -> i64 {
    0
}
```

Change `build_index_constituency_ilp_row` at
`index_constituency_persistence.rs:256` from
`buffer.at(TimestampNanos::new(row.trading_date_ist_nanos))?` to
`buffer.at(TimestampNanos::new(index_constituency_designated_ts_nanos()))?`.

Update the now-stale doc comments at `index_constituency_persistence.rs:62-69`
(DEDUP-key doc claims "one row per trading day" / "daily rebalance history") and
`:230-232` (claims "DEDUP fires on the (date, index, stock) key") to state the new
current-state semantics. The `trading_date_ist_nanos` field on
`IndexConstituencyRow` is RETAINED (callers still pass it; it is simply no longer
the stamped designated ts — it becomes unused-by-the-builder but kept for a stable
struct + zero churn at the two call sites). If clippy flags it unused, prefix-doc
it as "retained for caller compatibility; not the designated ts".

### B. Move `index_constituency` from DAY_PARTITIONED_TABLES → RETENTION_EXEMPT_TABLES

**CRITICAL caveat:** `index_constituency` is currently in
`DAY_PARTITIONED_TABLES` (`partition_manager.rs:55-88`, entry at `:61`). The
retention sweep `detach_old_partitions` DETACHes partitions older than 90 days.
With `ts` pinned to 0, ALL rows land in the `1970-01-01` partition, which is
>90 days old on every run — the sweep would target/detach it every cycle,
silently removing the live master.

Fix: mirror exactly how `instrument_lifecycle` is handled. It is a
`PARTITION BY DAY` current-state master that is EXEMPT from sweeping
(`partition_manager.rs:96` `RETENTION_EXEMPT_TABLES = &["instrument_lifecycle"]`).

- Remove the `"index_constituency"` entry from `DAY_PARTITIONED_TABLES`
  (`partition_manager.rs:61`).
- Add `"index_constituency"` to `RETENTION_EXEMPT_TABLES`
  (`partition_manager.rs:96`) → `&["instrument_lifecycle", "index_constituency"]`.
- Update the `RETENTION_EXEMPT_TABLES` doc comment (`:90-95`) to note the second
  exempt master (small, pinned-ts, never-swept current-state map).

This keeps the existing final-guard behaviour in `detach_old_partitions` (the
exempt set is already checked as the last line of defense at `:152-153`), so even
defense-in-depth is preserved.

### C. Update the partition-list ratchet test

`test_day_list_holds_the_audit_data_tables` (`partition_manager.rs:404-421`)
asserts `index_constituency` IS in `DAY_PARTITIONED_TABLES` (`:414`). After the
move that assertion is false. Remove `"index_constituency"` from the asserted-DAY
list in that test, and (matching the `test_no_table_in_both_lists` /
duplicate-table guard already present at `:395`) add a positive assertion that
`index_constituency` is now in `RETENTION_EXEMPT_TABLES` — so the test pins the
NEW contract instead of the old one.

### D. One-time marker-gated migration for the existing 1484 rows

After the ts pin lands, the next boot would write the 742-row set at `ts=0`, but
the legacy 1484 rows (at the two day-floored `ts` values from June28/June29)
remain — `DEDUP UPSERT` only collapses rows that share the FULL key including
`ts`, so the legacy-day rows never get overwritten by the `ts=0` write. They must
be cleared once.

Use the established **marker-gated boot-sweep pattern** (the same shape as
`shadow_persistence.rs:345-484`): on boot, check a sentinel marker file under the
data dir (e.g. `data/migrations/.index_constituency_ts_pin_v1.done`); if absent,
issue a one-time `TRUNCATE TABLE index_constituency` via `/exec`, then write the
marker file. TRUNCATE is supported and already used in this codebase
(`deploy/aws/lambda/operator-control/handler.py:730`). After truncate, the same
boot's normal `index_constituency` write rewrites the 742-row set at `ts=0`.

Wire the migration to run in the boot path BEFORE the `index_constituency` write
(so the order is truncate → write). Implementation lives in the `storage` crate
(a small `pub async fn migrate_index_constituency_truncate_once(...)` next to the
table's persistence module) and is called from the `app` boot wiring near the
existing `index_constituency` boot step. TRUNCATE-of-an-already-empty / already-
ts=0 table is harmless; the marker makes it strictly one-shot.

> NOTE: TRUNCATE here is SEBI-safe in the same sense `instrument_lifecycle`'s
> current-state model is — `index_constituency` is a daily-rebuilt current-state
> map (re-derived from the CSV every boot), NOT an append-only audit chain, so
> clearing it and rewriting from the source CSV loses no point-in-time record
> that wasn't already reproducible. (Per-day constituency HISTORY was never a
> tracked product feature — there is no date business column.)

---

## Edge Cases

| Case | Behaviour after fix |
|---|---|
| Two boots SAME trading day | `ts=0` for both → identical full key → DEDUP UPSERTs in place → 742 rows (was already 742 same-day; unchanged). |
| Two boots DIFFERENT trading days | Both write `ts=0` → DEDUP collapses to the latest 742 rows (THE FIX — was 1484). |
| Both feeds (Dhan + Groww) | `feed` stays in the key, `ts=0` for both → a Dhan row and a Groww row for the same (index, stock) are DISTINCT (different `feed`) and BOTH kept; same-feed cross-day rows collapse. Plumbing-only today (every writer stamps `dhan`). |
| Empty table on first ever boot | `append_index_constituency_rows` early-returns on empty input; migration TRUNCATE of an empty/absent-then-created table is a no-op; first real write lands 742 rows at `ts=0`. |
| Migration idempotency | Marker file gates the TRUNCATE to exactly once per environment; a second boot finds the marker and skips. Re-running is harmless (TRUNCATE of a current-state table that is immediately rewritten from CSV). |
| Partition behaviour | All rows in the `1970-01-01` partition; `index_constituency` now in `RETENTION_EXEMPT_TABLES` → never detached, exactly like `instrument_lifecycle`. The final-guard exempt check in `detach_old_partitions` is the backstop. |
| Constituent count drifts (741 vs 742) day-to-day | `ts=0` + DEDUP UPSERT means the table reflects the LATEST day's exact set; a stock dropped from NTM yesterday and absent today simply no longer has a row (current-state map — correct). |

---

## Failure Modes

| Failure | Detection | Handling |
|---|---|---|
| TRUNCATE fails (QuestDB rejects / DDL error) | `/exec` non-2xx body | Log `error!` (degrade-safe), DO NOT write the marker (so a later healthy boot retries the migration), DO NOT block boot. The legacy 1484 rows persist until the migration succeeds — over-count is cosmetic, never a correctness/trading risk. |
| Migration marker file lost (e.g. fresh data volume) | Marker absent on a subsequent boot | Migration re-runs (TRUNCATE again). Harmless: the boot immediately rewrites 742 rows from CSV at `ts=0`; the only effect is one extra TRUNCATE+rewrite cycle. |
| QuestDB DOWN at migration time | `/exec` connect error | Same as TRUNCATE-fails: log `error!`, skip marker write, do not block boot; the live feed + ticks path are untouched (this is cold-path master metadata). Next boot retries. |
| ILP write of the rewritten 742 rows fails post-truncate | `append_index_constituency_rows` returns `Err` | Existing behaviour: caller logs; table is empty for the window; next idempotent boot re-runs (DEDUP UPSERT safe). One-time-marker is already written by the time the write runs, so the migration itself is not re-triggered — only the normal idempotent write retries. |
| Someone re-adds `index_constituency` to `DAY_PARTITIONED_TABLES` | ratchet test (§Test Plan) | `test_day_list_holds_the_audit_data_tables` (updated) + the exempt-membership assertion fail the build. |

---

## Test Plan

1. **2-day regression test (the headline proof).** In the `storage` crate, a test
   that stamps two DIFFERENT `trading_date_ist_nanos` values through the SHARED
   builder `build_index_constituency_ilp_row` (e.g. June28-midnight-nanos and
   June29-midnight-nanos for the same (index, stock, feed)) and asserts the ILP
   buffer stamps the SAME designated `ts` (epoch 0) for both — proving the day
   value no longer reaches the key, so DEDUP collapses cross-day. Prefer a
   real-QuestDB integration test if the storage suite already has one
   (insert two days → `SELECT count(*)` → assert ONE row-set / 742-not-1484); if
   no live-QuestDB harness exists for this table, fall back to the pure-builder
   unit asserting the constant is stamped (the builder is the single funnel for
   both feeds, so the unit fully covers the root cause). The pure builder is
   already unit-tested (`build_index_constituency_ilp_row` is the documented unit
   target), so this extends that existing coverage.

2. **Constant ratchet.** `test_index_constituency_designated_ts_is_epoch_zero` —
   asserts `index_constituency_designated_ts_nanos() == 0` (mirrors the existing
   `test_build_snapshot_timestamp_returns_epoch_zero_constant` /
   lifecycle constant tests).

3. **Retention-exempt ratchet.** Assert `RETENTION_EXEMPT_TABLES` contains
   `"index_constituency"` AND `DAY_PARTITIONED_TABLES` does NOT — pinning the
   partition-list move so a regression fails the build. The existing
   `test_no_table_in_both_lists` / duplicate guard at `partition_manager.rs:395`
   continues to ensure no table is in both lists.

4. **Update `test_day_list_holds_the_audit_data_tables`**
   (`partition_manager.rs:404-421`): remove the `"index_constituency"` entry from
   the asserted DAY list; add the positive exempt-membership assertion from (3).

5. **Migration idempotency unit.** Test the marker-gate predicate (pure logic:
   given marker-present → skip; marker-absent → run) so the one-shot semantics are
   covered without live QuestDB.

6. **Meta-guards already in force (no change, must stay green):**
   `dedup_segment_meta_guard::every_persisted_table_dedup_key_must_include_feed`
   and `per_feed_market_data_dedup_keys_must_include_feed` — the DEDUP key string
   is unchanged (we only change the stamped VALUE of `ts`, not the key columns),
   so these continue to pass.

---

## Rollback

- **Code rollback is a clean revert:** restore `buffer.at(...)` at
  `index_constituency_persistence.rs:256` to `row.trading_date_ist_nanos`, remove
  `index_constituency_designated_ts_nanos()`, move `"index_constituency"` back
  from `RETENTION_EXEMPT_TABLES` → `DAY_PARTITIONED_TABLES`, and revert the
  `test_day_list_holds_the_audit_data_tables` edit. Single-commit revert.
- **The migration is one-shot, marker-gated, and forward-only.** A code rollback
  does NOT need to "undo" the TRUNCATE — the table simply rebuilds from CSV on
  the next boot under whichever ts rule is active (reverting to day-floored ts
  would resume per-day accumulation, which is the pre-fix behaviour, not a
  corruption). No data is unrecoverable: `index_constituency` is fully
  re-derivable from the niftyindices/Dhan CSV on every boot.
- DEDUP key columns are untouched throughout, so no schema migration / no
  populated-table DROP is ever involved (SEBI-safe).

---

## Observability

- **One-time migration boot log.** On the marker-absent path, emit a single
  `info!` line on the TRUNCATE: e.g.
  `info!(table = "index_constituency", "one-time ts-pin migration: truncated legacy day-floored rows")`,
  followed (after the rewrite) by the existing success log that reports the row
  count written (742). On TRUNCATE failure emit `error!` (degrade-safe, routes to
  Telegram via the 5-sink pipeline) with a stable message; marker NOT written so
  the next boot retries.
- **No new counter required** — this is a one-shot cold-path migration, not a
  recurring failure mode; the boot `info!`/`error!` lines plus the existing
  `index_constituency` write success/error logging are sufficient. (If the team
  wants a recurring signal, the existing per-write error path already logs.)
- **Operator verification query** (manual, post-deploy): after the migrated boot,
  `SELECT ts, count(*) FROM index_constituency GROUP BY ts` returns exactly ONE
  group at `1970-01-01` with ~742 rows — the GROUP-BY-ts proof the operator used
  to confirm the bug now confirms the fix.

---

## Per-Item Guarantee Matrix (15-row + 7-row, per per-wave-guarantee-matrix.md)

### 15-row "100% everything"

| Demand | Proof for this fix |
|---|---|
| 100% code coverage | new pure const + builder change are unit-covered; migration marker-gate predicate unit-tested |
| 100% audit coverage | reuses the existing `index_constituency` master table (DEDUP incl. feed); no new audit surface |
| 100% testing coverage | smoke + 2-day regression + constant ratchet + retention-exempt ratchet + partition-list test update + migration idempotency |
| 100% code checks | banned-pattern, pub-fn-test, pub-fn-wiring (migration fn wired into boot), plan-verify, dedup meta-guard |
| 100% code performance | cold-path daily build + one-shot migration (NOT hot path) — no per-tick alloc; `index_constituency_designated_ts_nanos()` is a const fn |
| 100% monitoring | boot `info!`/`error!` on the one-time truncate; existing per-write logging unchanged |
| 100% logging | `error!` (degrade-safe) on TRUNCATE/QuestDB failure; `info!` on migration + count; never logs secrets |
| 100% alerting | TRUNCATE/QuestDB failure `error!` routes to Telegram via the 5-sink error pipeline |
| 100% security | no secrets touched; ILP strings sanitized by the reused builder; TRUNCATE is a fixed DDL string (no injection surface) |
| 100% security hardening | no new external input; marker path is fixed under the data dir; no panic paths (degrade-safe) |
| 100% bug fixing | adversarial 3-agent review BEFORE + AFTER (hot-path, security, hostile) on the diff |
| 100% scenarios covering | 7 edge cases + 4 failure modes enumerated above |
| 100% functionalities covering | every new pub fn (const + migration) has a test + a call site (boot wiring) |
| 100% code review | self-review + the 3-agent pass on the diff |
| 100% extreme check | ratchets (constant ==0, retention-exempt membership, partition-list test, dedup meta-guard) fail the build on regression |

### 7-row "Resilience"

| Demand | Honest envelope for this fix |
|---|---|
| Zero ticks lost | N/A — `index_constituency` is current-state master metadata, NOT the ticks path; introduces no tick-drop path |
| WS never disconnects | N/A — no WebSocket code touched |
| Never slow/locked/hanged | one-shot cold-path migration + cold-path daily build; never blocks the hot path or boot (degrade-safe) |
| QuestDB never fails | ABSORB: TRUNCATE/write failure is degrade-safe (`error!` + skip marker + do-not-block-boot); next idempotent boot re-runs |
| O(1) latency | `index_constituency_designated_ts_nanos()` is O(1) const; per-row build O(1); whole daily build is cold-path O(n) over the constituency set (flagged O(n), honestly — not a per-tick path) |
| Uniqueness + dedup | composite `(index_name, security_id, exchange_segment, feed)` + pinned `ts=0` — DEDUP fires on the business key; cross-day duplicates eliminated; Dhan + Groww rows coexist via `feed` |
| Real-time proof | boot `info!`/`error!` + the operator `GROUP BY ts` query (one group at 1970-01-01, ~742 rows) proves the fix in real time |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: pinning the
`index_constituency` designated `ts` to constant epoch 0 (mirroring
`lifecycle_designated_ts_nanos()`) makes its DEDUP UPSERT fire on the business key
`(index_name, security_id, exchange_segment, feed)` alone, collapsing the cross-day
742-per-day accumulation to a single current-state ~742-row set; the table is moved
to `RETENTION_EXEMPT_TABLES` so the `1970-01-01` partition is never swept (exactly
like `instrument_lifecycle`); the legacy 1484 rows are cleared by a one-shot
marker-gated `TRUNCATE` that is degrade-safe (a failure logs `error!`, leaves the
marker unwritten so a healthy boot retries, and never blocks boot or touches the
ticks/order path). The DEDUP key columns are unchanged, so no schema migration and
no populated-table DROP is involved (SEBI-safe), and the meta-guards stay green.
Beyond the envelope, `index_constituency` is fully re-derivable from the
niftyindices/Dhan CSV on every boot, so no data is unrecoverable.
