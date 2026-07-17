# Implementation Plan: Drop-Sweep Two Dead REST-Era Tables

**Status:** DRAFT
**Date:** 2026-07-17
**Approved by:** pending

## Design

The boot-time idempotent `DROP TABLE IF EXISTS` sweep in the
`tickvault-storage` crate (`crates/storage/src/shadow_persistence.rs`,
`RETIRED_QUESTDB_TABLES`) converges every deployment's live QuestDB schema to
the 24-table KEEP set with zero manual migration. Two dead REST-era feed
tables — `deep_market_depth` (the retired 20/200-level depth writer target)
and `movers_1s` (the deleted movers pipeline base table) — still linger on
older prod/dev volumes but have no writer on `main`. This change appends
exactly those two string literals to the `RETIRED_QUESTDB_TABLES` array
(length `[&str; 12]` → `[&str; 14]`) so the existing boot sweep loop drops
them automatically. The drop loop itself is unchanged: it already iterates the
array (`for table in RETIRED_QUESTDB_TABLES`), so adding array entries is the
whole behavioural change.

## Edge Cases

Both tables may be entirely absent on a fresh volume — `DROP TABLE IF EXISTS`
is a no-op in that case, so a clean install is unaffected. A table that is
present but empty is dropped identically. Neither name collides with the
KEEP-24 candle tables (`candle_table_names()` only emits `candles_<tf>`),
`ticks`, `historical_candles`, `option_chain_minute_snapshot`, or any
`*_audit` table — verified by the existing exclusion guard, which iterates the
full array. Re-running the sweep on an already-clean schema is idempotent.

## Failure Modes

If QuestDB is unreachable at boot, the existing sweep already logs and degrades
per the surrounding module contract; adding two more DDLs does not introduce a
new failure surface. A malformed table name cannot occur — the literals are
compile-time constants. The only regression risk is accidentally listing a
protected table; that is mechanically prevented by the count guard
(`len() == 14`) plus the keep/audit exclusion guard, both of which fail the
build on drift.

## Test Plan

Run the two inline guard tests in `tickvault-storage`:
`test_retired_questdb_tables_count_is_fourteen_and_unique` (asserts the array
is exactly 14 entries and all unique) and
`test_retired_questdb_tables_excludes_keep_set_and_audit` (asserts no KEEP-24
candle table, no `ticks`/`historical_candles`/`option_chain_minute_snapshot`,
and no `*_audit`/`order_audit` name appears in the array). Both must report
`test result: ok`. Also run the banned-pattern scanner.

## Rollback

Revert the single commit: the change is confined to
`crates/storage/src/shadow_persistence.rs` (array literals + one count guard).
Reverting restores the `[&str; 12]` array and the `count_is_twelve` guard name.
No data is destroyed by a rollback — a table already dropped simply stays
dropped; nothing recreates it, and neither table has a writer on `main`.

## Observability

The boot sweep already emits its DDL execution through the module's existing
logging path; the two new `DROP TABLE IF EXISTS` statements ride that same
surface with no new metric or alert required. Success is observable as the
tables' absence in the live QuestDB schema after boot (queryable via the
tickvault-logs `questdb_sql` tool). No new `ErrorCode`, counter, or audit
table is introduced.

## Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 rows of the
guarantee matrix and all 7 rows of the resilience-demand matrix apply to this
single item — a 100% guarantee inside the tested envelope, with ratcheted
regression coverage (the two inline `RETIRED_QUESTDB_TABLES` guard tests + the
keep/audit-exclusion guard); no runtime path is touched. Every applicable row
is covered by the two inline guard tests (code/testing/extreme-check coverage),
the audit-exclusion guard (uniqueness + SEBI protection), and the confined
single-file diff (no hot-path, no new tick-drop path, composite-key/DEDUP
untouched).
