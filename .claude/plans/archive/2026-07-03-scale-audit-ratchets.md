# Implementation Plan: groww_scale_audit ratchet fixes (feed-in-dedup-key + retention decision)

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — standing directive: main push CI is RED since #1387 (`Test (storage)` fails `dedup_segment_meta_guard` + `partition_retention_coverage_guard` against the new `groww_scale_audit` table); "FIX both properly (no test weakening)" — add `feed` to the schema/DEDUP key/writer row and an explicit retention decision, drive to merge.

> **Guarantee matrices:** this hotfix cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row) per the
> CLAUDE.md "or cross-reference it" clause — cold-path audit-table plumbing only,
> no hot-path change, no new failure mode, both build-failing ratchets are the
> extreme-check proof.

## Design

Two main-blocking ratchet failures, both caused by PR #1387's new
`groww_scale_audit` table missing two repo-wide table contracts:

1. **feed-in-key EVERYWHERE (operator override 2026-06-28,
   `data-integrity.md`):** every persisted QuestDB table's DEDUP UPSERT key must
   include `feed`. `crates/storage/src/groww_scale_audit_persistence.rs` gets:
   `feed SYMBOL` added to the CREATE DDL, `feed` appended to
   `DEDUP_KEY_GROWW_SCALE_AUDIT`, `feed` appended to `INSERT_COLUMN_LIST`, and a
   new `GROWW_SCALE_AUDIT_FEED = "groww"` constant stamped as the last value of
   every row tuple (the ladder is a Groww-only subsystem per §34 of
   `groww-second-feed-scope-2026-06-19.md`, so the value is a compile-time
   constant — no `GrowwScaleAuditRow` field change, zero app-crate API impact).
   `ensure_groww_scale_audit_table` gains the canonical schema self-heal
   (mirrors `tick_conservation_audit_persistence.rs`): `ALTER TABLE … ADD COLUMN
   IF NOT EXISTS feed SYMBOL` → `UPDATE … SET feed='groww' WHERE feed IS NULL` →
   `ALTER TABLE … DEDUP ENABLE UPSERT KEYS(…)` — in THAT order so a re-keyed
   table upserts over a legacy NULL-feed row instead of duplicating it.

2. **Retention decision (`partition_retention_coverage_guard`):**
   `groww_scale_audit` is added to `DAY_PARTITIONED_TABLES` in
   `crates/storage/src/partition_manager.rs` — the same retention class as its
   sibling audit tables (`ws_event_audit`, `tick_conservation_audit`): DAY
   partitioned, SEBI-audit class, DETACH past the hot window (never DROP).

No test is weakened — the two ratchets pass because the CONTRACT is now met.

## Edge Cases
- Table already created by a pre-fix boot (no `feed` column): self-heal ADDs the
  column, backfills NULL→'groww', re-enables DEDUP with the new key — idempotent
  on every subsequent boot (QuestDB ignores existing ADDs).
- Rows written pre-fix with NULL feed: backfilled to 'groww' BEFORE the DEDUP
  re-enable, so re-appended rows upsert in place (no duplicates).
- Rehydration reader (`rehydrate_select_sql` / `parse_scale_audit_dataset`)
  selects columns by NAME (`ts_micros, to_conns, outcome`) — appending `feed` to
  the table/INSERT does not shift its positions; unchanged.
- `groww_scale_audit` sits at the END of `DAY_PARTITIONED_TABLES`, so the sweep
  order of existing tables is unchanged.

## Failure Modes
- Self-heal DDL failure (QuestDB down/non-2xx): each statement logs `error!`
  and continues — same degrade family as the existing CREATE path (table may
  lack DEDUP keys until next boot; the append is best-effort GROWW-SCALE-04,
  never gates a ladder decision).
- Retention sweep DETACHes only >retention_days partitions of
  `groww_scale_audit`; the table is never DROPped (SEBI).
- No new panic path: all changes are string constants + one extra literal in a
  pure formatter.

## Test Plan
- Extended unit tests in `groww_scale_audit_persistence.rs`:
  `test_dedup_key_includes_designated_timestamp` (now also asserts `feed`),
  `test_groww_scale_audit_ddl_contains_expected_columns` (now asserts
  `feed SYMBOL`), NEW `test_feed_constant_is_groww_and_stamped_in_tuple`.
- The two ratchets themselves are the proof:
  `dedup_segment_meta_guard::every_persisted_table_dedup_key_must_include_feed`
  and
  `partition_retention_coverage_guard::every_storage_table_constant_has_a_retention_decision`.
- `cargo test -p tickvault-storage` fully green (lib 697 passed; both ratchet
  files green: 7 + 3 passed). `cargo check -p tickvault-app` clean (no API
  change). `cargo fmt --check` clean.

## Rollback
`git revert` of the single commit — additive schema/list changes only. QuestDB
tolerates the extra `feed` column on old code (ILP/exec inserts name columns
explicitly), and removing the retention-list entry only stops sweeping the
table. No data migration to undo.

## Observability
- No new metric/event/ErrorCode: the table's write failures remain
  GROWW-SCALE-04 (Medium, best-effort — `groww-scale-error-codes.md` §4); the
  self-heal failures log at `error!` (Telegram-routable) exactly like the
  sibling audit tables.
- The retention sweep's existing logging now covers `groww_scale_audit`
  partitions like every other DAY audit table.

## Plan Items
- [x] Add `feed` to `groww_scale_audit` schema + DEDUP key + writer row ('groww' constant) + idempotent self-heal in the ensure fn
  - Files: crates/storage/src/groww_scale_audit_persistence.rs
  - Tests: test_dedup_key_includes_designated_timestamp, test_groww_scale_audit_ddl_contains_expected_columns, test_feed_constant_is_groww_and_stamped_in_tuple
- [x] Add `groww_scale_audit` retention decision (DAY_PARTITIONED_TABLES)
  - Files: crates/storage/src/partition_manager.rs
  - Tests: every_storage_table_constant_has_a_retention_decision

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Greenfield boot | CREATE ships `feed` column + feed-in-key DEDUP directly |
| 2 | Boot against pre-fix table | self-heal ADD COLUMN → backfill 'groww' → DEDUP re-enable, idempotent |
| 3 | Replayed/duplicate append | same `(trading_date_ist, ts, from_conns, to_conns, outcome, feed)` key → collapsed (idempotent) |
| 4 | Retention sweep run | >hot-window `groww_scale_audit` partitions DETACHed, never DROPped |
