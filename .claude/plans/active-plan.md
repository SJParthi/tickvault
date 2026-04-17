# Implementation Plan: Instrument Tables — Single-Row-Per-Instrument Redesign

**Status:** APPROVED
**Date:** 2026-04-17
**Approved by:** Parthiban (2026-04-17)

## Problem (Parthiban, 2026-04-17)

Current design: snapshot tables accumulate one row per day per instrument.
Running on Day 1 + Day 2 = 2x rows. Dashboard showed 442 underlyings (2x 221)
and 207,796 contracts (2x ~104k). This is fragile — every new query must
remember the `WHERE timestamp = max(timestamp)` filter or it shows duplicates.

**Correct design per Parthiban:**
- Tables must have UNIQUE rows always
- Same-day OR cross-day reload = NO duplicates
- New contracts auto-added as unique rows
- Expired contracts auto-deactivated (NOT deleted, for SEBI audit)

## Target Design — Lifecycle Columns, No Timestamp in Dedup Key

### `fno_underlyings`
| Column | Type | Purpose |
|---|---|---|
| underlying_symbol | SYMBOL | **Business key (DEDUP)** |
| status | SYMBOL | `active` \| `expired` |
| underlying_security_id | LONG | From CSV |
| price_feed_segment | SYMBOL | From CSV |
| price_feed_security_id | LONG | From CSV |
| derivative_segment | SYMBOL | From CSV |
| kind | SYMBOL | NseIndex / BseIndex / Stock |
| lot_size | LONG | From CSV |
| contract_count | LONG | Current active contracts |
| first_seen_date | TIMESTAMP | When row was first inserted |
| last_seen_date | TIMESTAMP | Last day CSV had this symbol |
| expired_date | TIMESTAMP | null if active, date if expired |
| ts | TIMESTAMP (designated) | = last_seen_date |

**DEDUP key:** `underlying_symbol` (one row per symbol, period)

### `derivative_contracts`
Similar lifecycle columns. DEDUP: `security_id, underlying_symbol`.

### `subscribed_indices`
Similar lifecycle columns. DEDUP: `security_id`.

### `instrument_build_metadata`
UNCHANGED — build history table, one row per run is correct by design.

## Write Semantics (daily build)

```
1. Load today's CSV → universe builder → FnoUniverse (today's active set)
2. For each instrument in today's universe:
     UPSERT (DEDUP on business key):
       - new row → INSERT with status='active', first_seen_date=today, last_seen_date=today
       - existing row → UPDATE fields, status='active', last_seen_date=today, expired_date=null
3. For each row in DB with status='active' not in today's universe:
     UPDATE: status='expired', expired_date=today
```

**Idempotency proofs:**
- Run same day N times: identical final state (DEDUP + deterministic inputs)
- Run cross-day: rows UPSERTed, never duplicated
- Expired contract reappears: reactivated (status='active', expired_date=null)
- Corporate action (lot_size change): UPDATE single row, last_seen bumped

## Plan Items

- [ ] **1. Update DDLs** in `crates/storage/src/instrument_persistence.rs`
  - Add `status`, `first_seen_date`, `last_seen_date`, `expired_date` columns
  - Change DEDUP keys to business key only (no timestamp-in-key)
  - Tests: `test_ddl_status_column`, `test_ddl_lifecycle_dates`, `test_dedup_business_key`

- [ ] **2. Rewrite `persist_instrument_snapshot` → `persist_instrument_universe`**
  - UPSERT today's universe
  - Call `mark_missing_as_expired(today)` to flip absent rows to expired
  - Remove `build_snapshot_timestamp()` (no longer a PK component)
  - Tests: `test_upsert_idempotent_same_day`, `test_cross_day_no_duplication`,
    `test_new_added_as_active`, `test_missing_marked_expired`, `test_reappearing_reactivated`

- [ ] **3. Update Grafana dashboards** — remove timestamp filters
  - `deploy/docker/grafana/dashboards/market-data.json`
  - Count queries: `SELECT count() FROM fno_underlyings WHERE status = 'active'`
  - Table queries: add `WHERE status = 'active'` for operational views
  - Guard test (`grafana_dashboard_snapshot_filter_guard`) updated:
    now checks for `WHERE status` filter instead of timestamp filter

- [ ] **4. Update `verify_instrument_row_counts`**
  - Count `WHERE status = 'active'` only
  - Tests: `test_verify_active_count_matches_universe`

- [ ] **5. Rewrite I-P1-08 rule** in `.claude/rules/project/gap-enforcement.md`
  - OLD: "cross-day snapshot accumulation by design"
  - NEW: "single-row-per-instrument with lifecycle status, UPSERT semantics"

- [ ] **6. Migration script** `scripts/migrate-instrument-tables.sql`
  - Keep only latest snapshot per business key
  - Add lifecycle columns, set status='active' for kept rows
  - Rename old tables `*_pre_migration` (kept 7 days, then drop)

- [ ] **7. Update delta_detector.rs**
  - Delta events derived from DB status transitions, not universe-vs-universe diff
  - All 29 existing delta_detector tests updated/kept

- [ ] **8. Update docs**
  - `CLAUDE.md` — codebase structure section
  - `docs/dhan-ref/09-instrument-master.md` — add "lifecycle" section
  - `.claude/rules/dhan/instrument-master.md` — update rules

- [ ] **9. Add mechanical uniqueness guard**
  - `crates/storage/tests/instrument_uniqueness_guard.rs`
  - Property test: simulate N days × M runs per day, assert
    `count(distinct underlying_symbol) == count(*)` always

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Day 1 run once (221 underlyings) | 221 rows, all active |
| 2 | Day 1 run 100 times (same CSV) | Still 221 rows |
| 3 | Day 2 run (220 same + 2 new + 1 expired overnight) | 223 rows total: 220 active, 1 expired, 2 new active |
| 4 | Day 3 run (expired one reappears) | Row reactivated: status=active, expired_date=null |
| 5 | Dashboard count query | Returns exact active count, no filter needed |
| 6 | Historical query "what was active on 2026-03-01?" | `WHERE first_seen_date <= '2026-03-01' AND (expired_date IS NULL OR expired_date > '2026-03-01')` |
| 7 | SecurityId reused across underlyings | SecurityIdReused event fires (compound identity) |
| 8 | Corporate action: lot_size changes | UPDATE single row, last_seen bumped, delta event emitted |
| 9 | SEBI audit "list all contracts traded since 2026-01-01" | Join with orders table on security_id |
| 10 | Migration from old design | One-time script: keep latest snapshot per key |

## Rollback

- Old tables kept as `*_pre_migration` for 7 days
- Revert commit → restore old DDL → copy data back from `*_pre_migration`

## Approval Required

**This is a schema-level redesign.** Estimated effort: 1 focused session
(~3 hours). Touches: storage DDL, persistence functions, delta detector,
Grafana dashboards, 5 docs, ~40 tests.

**I will NOT touch any code until you approve this plan.**
