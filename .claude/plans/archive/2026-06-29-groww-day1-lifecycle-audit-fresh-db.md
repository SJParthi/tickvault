# Implementation Plan: Groww Day-1 lifecycle_audit on fresh DB + constituency write integrity

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session (audit-driven, code-cited)

Crates touched: **tickvault-core**
(`crates/core/src/feed/groww/shared_master_writer.rs`).

On a fresh-clone / fresh-QuestDB run the operator observed:
`instrument_lifecycle` feed='groww' = 767 rows (good) but
`instrument_lifecycle_audit` feed='groww' = 0 and `index_constituency`
feed='groww' = 0. This plan fixes the real latent fresh-DB ordering gap that
keeps the Groww audit-history empty on Day-1, and pins the constituency-write
integrity. Everything here is cold-path master/metadata persistence — NOTHING
touches ticks / dedup / persist of live data / orders.

## Plan Items

- [x] **FIX 2 — Day-1 `instrument_lifecycle_audit` (feed='groww') is empty on a
  fresh DB.** Root cause: `persist_groww_instruments` calls
  `emit_groww_lifecycle_audit` BEFORE `ensure_instrument_lifecycle_table`. The
  audit's prior-snapshot read (`load_prev_groww_lifecycle` →
  `SELECT … FROM instrument_lifecycle WHERE feed='groww'`) runs against a table
  that does not exist yet on a fresh DB → QuestDB returns HTTP 4xx
  `table does not exist` → the loader bails → `emit_groww_lifecycle_audit` takes
  its degrade-safe early return → 0 audit rows. Day-1 `appeared` classification
  in `build_groww_audit_rows` is correct but never runs. Fix (both, defence in
  depth): (a) a pure testable `is_table_missing_response(is_client_error, body)`
  classifier so `load_prev_groww_lifecycle` treats a 4xx "table does not exist"
  as an EMPTY prior (Day-1 → all `appeared`), NOT a hard bail; genuine outages
  (5xx / refused / other body) still bail degrade-safe. (b) move the idempotent
  `ensure_instrument_lifecycle_table` AHEAD of the audit emit so the read targets
  a real (empty) table even on the very first boot. §24 audit-first preserved
  (emit still precedes the DATA UPSERT).
  - Files: `crates/core/src/feed/groww/shared_master_writer.rs`
  - Tests: `test_is_table_missing_response_classifies_questdb_missing_table`,
    `test_is_table_missing_response_rejects_genuine_outage`,
    `test_fresh_db_empty_prior_yields_day1_appeared_audit_rows`,
    `test_persist_ensures_lifecycle_table_before_audit_emit`

- [x] **FIX 3 — `index_constituency` (feed='groww') = 0 — verdict: stale-binary,
  not a code gap.** The constituency persist already calls
  `ensure_index_constituency_table` BEFORE `append_index_constituency_rows`, so
  it does NOT share the Fix-2 read-before-create ordering bug — it creates its
  table first, then writes. The build filter (`kind == Ltp && symbol_name.is_some()`)
  produces one row per resolved stock; the resolver sets `symbol_name: Some(_)`,
  so a resolved Groww stock set yields ~742 rows on the first boot. The
  constituency write was added in PR #1230; the operator's running binary
  predated it (stale binary). No production change needed for Fix 3; regression
  tests pin the ensure-before-append ordering + non-empty build so a future
  refactor cannot silently regress it.
  - Files: `crates/core/src/feed/groww/shared_master_writer.rs` (tests only)
  - Tests: `test_persist_ensures_constituency_table_before_append`,
    `test_constituency_build_is_nonempty_for_resolved_stock_set`

- [x] **FIX 1 — archive the merged #1262 plan; replace active-plan.md.** The stale
  `active-plan.md` belonged to the already-merged feed-health PR #1262 and lacked
  the guarantee matrix, failing the storage CI guard
  `test_every_wave_5_item_carries_guarantee_matrix`. Archived to
  `.claude/plans/archive/2026-06-29-groww-feed-health-trading-day-aware.md`
  (Status: VERIFIED) per `plan-enforcement.md` Phase 4; this file is the new plan.
  - Files: `.claude/plans/active-plan.md`,
    `.claude/plans/archive/2026-06-29-groww-feed-health-trading-day-aware.md`
  - Tests: `cargo test -p tickvault-storage --test per_item_guarantee_matrix_guard`,
    `bash .claude/hooks/per-item-guarantee-check.sh`, `bash .claude/hooks/plan-gate.sh`

## Design

The defect lives entirely in the **cold-path Groww master writer** ordering. The
real-time tick/dedup/persist-of-live-data/order paths are untouched.

1. **Read-vs-create ordering (Fix 2 root).** The Groww writer's audit prior-read
   ran before the table DDL. On a fresh DB the read hits a missing table. The Dhan
   side never sees this because its boot creates the lifecycle table (Step 6b)
   long before any read. We make the Groww writer self-sufficient by ensuring the
   table FIRST, and additionally make the reader classify a table-missing 4xx as
   an empty prior. Either guard alone fixes Day-1; together they are robust to a
   future reorder.

2. **Pure classifier over the read.** `is_table_missing_response` is a pure,
   feature-gated, unit-tested function: `is_client_error && body~="does not exist
   | table not found"`. A 5xx (server outage) or any unrelated 4xx body is NOT
   table-missing → the loader still bails → the existing GROWW-MASTER-01
   degrade-safe arm logs+counts+returns (no audit rows that boot, re-emitted next
   boot). This preserves the honest no-false-OK behaviour for genuine outages.

3. **Constituency (Fix 3).** Already correct: ensure-before-append + a build that
   emits one row per resolved stock. The "0 rows" the operator saw is a
   stale-binary artifact of a binary predating PR #1230's constituency write. We
   add source-scan + build-nonempty regression tests; no production change.

## Edge Cases

- Fresh DB, table absent: the read returns an empty prior → every Groww master
  entry is classified `appeared` → audit rows ARE written (the regression).
- Genuine QuestDB outage (5xx / connection refused): NOT table-missing → loader
  bails → degrade-safe GROWW-MASTER-01 (no audit rows that boot, no panic, next
  boot re-emits via DEDUP UPSERT). No false-OK.
- A 4xx whose body is an unrelated rejection (syntax error): NOT table-missing →
  bails, so a real query bug is never silently swallowed.
- Case/format variance in QuestDB's body (`TABLE DOES NOT EXIST`,
  `table does not exist [table=...]`, `table not found`): all matched
  case-insensitively.
- §24 audit-first invariant: the audit emit still runs BEFORE the lifecycle DATA
  UPSERT — only the (cheap, idempotent) DDL moved ahead of the read. Pinned by
  `test_groww_audit_emitter_is_degrade_safe_and_wired` +
  `test_persist_ensures_lifecycle_table_before_audit_emit`.
- §27 dry-run isolation: dry-run still skips all appends (the emit is reached only
  on a live run); unchanged.

## Failure Modes

- QuestDB ILP/HTTP down at append time: existing GROWW-MASTER-01 degrade-safe arms
  (log + `tv_groww_master_persist_errors_total{stage}` + return) — never aborts
  the feed, ticks, or orders. Unchanged.
- Misclassifying an outage as table-missing → would silently lose audit rows on a
  real outage. Prevented by the `is_client_error` gate (5xx never matches) +
  unit tests pinning the genuine-outage rejection.
- Constituency "0 rows" recurrence: a future refactor reordering ensure/append, OR
  a build filter regression, is caught by the two Fix-3 regression tests.
- No new allocation, no new lock on any hot path — all changes are cold-path
  master/metadata persistence + the once-per-persist prior read.

## Test Plan

Pure functions make every fix testable without a live QuestDB:
- `shared_master_writer.rs` unit tests (feature `daily_universe_fetcher`): the
  table-missing classifier (positive + genuine-outage rejection), the Day-1
  empty-prior → all-appeared end-to-end on the pure path, the ensure-before-emit
  and ensure-before-append source-scan ratchets, and the constituency
  build-nonempty.
- Run `cargo test -p tickvault-core --features daily_universe_fetcher --lib
  shared_master_writer` (all green). Run
  `cargo test -p tickvault-storage --test per_item_guarantee_matrix_guard`,
  `bash .claude/hooks/banned-pattern-scanner.sh`,
  `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all`,
  `bash .claude/hooks/plan-gate.sh "$PWD"`,
  `bash .claude/hooks/pre-push-gate.sh`. No flush/persist phrase is downgraded
  (the GROWW-MASTER-01 arms stay `error!`), so `error_level_meta_guard.rs` is
  unaffected.

## Rollback

The change is contained to one cold-path module. To roll back: revert the single
PR. No schema change, no migration, no live-data path touched. A revert returns
the system to the pre-PR behaviour (Day-1 Groww audit empty on a fresh DB),
never to a worse state. The constituency path is unchanged in production, so a
revert cannot affect it.

## Observability

- No new `error!` codes needed — the existing GROWW-MASTER-01 +
  `tv_groww_master_persist_errors_total{stage}` cover the degrade paths. The
  table-missing case logs a single `info!` ("instrument_lifecycle table absent
  (fresh DB) — treating as empty prior (Day-1, all appeared)") and the existing
  audit success `info!` ("instrument_lifecycle_audit transition rows persisted")
  now fires with a non-zero count on Day-1.
- After this PR a fresh database DOES populate `instrument_lifecycle_audit`
  (feed='groww') with one `appeared` row per resolved Groww instrument on the
  first boot, and `index_constituency` (feed='groww') is written by current main
  (the prior 0 was a stale binary) — both directly queryable via
  `mcp__tickvault-logs__questdb_sql`.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Fresh DB, first Groww boot, table absent | read → empty prior → Day-1 `appeared` audit rows written (non-zero) |
| 2 | Genuine QuestDB 5xx outage during the prior read | NOT table-missing → degrade-safe GROWW-MASTER-01, no rows, next boot re-emits |
| 3 | 4xx with unrelated body (syntax error) | NOT table-missing → bails (real bug surfaced), no silent swallow |
| 4 | Second-day boot, prior rows exist | normal diff (appeared/updated/expired), no spurious all-appeared |
| 5 | Resolved Groww stock set, fresh DB | constituency rows written (ensure-before-append; one row per resolved stock) |
| 6 | Dry-run boot | all appends skipped (§27 isolation), unchanged |

## Per-item guarantee matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 rows of the
15-row "100% everything" matrix and all 7 rows of the Resilience Demand Matrix
apply to every item in this plan. Honest envelope: the fixes are confined to the
**cold-path Groww master/metadata writer** — they add no hot-path allocation and
touch no tick / dedup of live data / persist-of-live-data / order path, so the
DHAT zero-alloc and Criterion p99 rows are `N/A — cold-path master write`.
Coverage is proven by the pure-function unit tests named per item
(`shared_master_writer.rs`); composite-key uniqueness
`(security_id, exchange_segment, feed)` per I-P1-11 + the feed-in-key DEDUP keys,
and the zero-tick-loss ring→spill→DLQ chain, are unchanged. Any honest "100%"
claim in this plan is 100% inside the tested envelope, with ratcheted regression
coverage by those unit tests.
