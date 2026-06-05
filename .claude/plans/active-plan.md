# Implementation Plan: Partition-retention coverage fix (storage cost runaway)

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban ("proceed with everything fix everything … approved")
**Crate(s) touched:** `storage`

## Context (verified, no illusion)

The partition manager's retention sweep (`detach_old_partitions`) iterates two
hard-coded lists. Verified against the code:
- `HOUR_PARTITIONED_TABLES` names **deleted** tables (greeks/depth/movers/option_chain — modules gone).
- `DAY_PARTITIONED_TABLES` = `["candles_1s"]` only.
- The **live growing** `PARTITION BY DAY` tables — the 10 `candles_*_shadow`, plus
  `instrument_fetch_audit`, `instrument_lifecycle_audit`, `option_chain_minute_snapshot`,
  `prev_day_ohlcv`, `cross_verify_1m_audit` — are in **neither list**, so their old
  partitions are **never swept** → unbounded active-table growth.
- `instrument_lifecycle` is `PARTITION BY DAY` but is the **never-delete SEBI
  point-in-time** table — it MUST be excluded from sweeping.

**Honest boundary (stated, not hidden):** `detach_old_partitions` uses
`ALTER TABLE … DETACH PARTITION` — it moves old partitions to a local `detached/`
dir (SEBI-safe, no DROP). There is **no S3 archiver module** in the codebase, so
detach alone does **not** free EBS bytes — it bounds the *active* table and is the
prerequisite for archival. Actually freeing EBS (S3 upload + local cleanup of
detached partitions) is a SEPARATE piece that needs AWS provisioning; this PR does
**not** claim to free EBS, only to fix the stale-coverage bug + prevent its return.

## Plan Items

- [x] Item 1 — Fix the two lists to the live reality; add a documented exempt set
  - Files: `crates/storage/src/partition_manager.rs`
  - Tests: `partition_manager::tests::test_*` (coverage + SEBI-exclusion)
- [x] Item 2 — Anti-staleness meta-guard (can't-go-stale ratchet)
  - Files: `crates/storage/tests/partition_retention_coverage_guard.rs`
  - Tests: `every_storage_table_constant_has_a_retention_decision`

## Design

1. `HOUR_PARTITIONED_TABLES = ["ticks"]` (the only live HOUR table).
2. `DAY_PARTITIONED_TABLES` = the 10 `candles_*_shadow` + `instrument_fetch_audit`
   + `instrument_lifecycle_audit` + `option_chain_minute_snapshot` + `prev_day_ohlcv`
   + `cross_verify_1m_audit`.
3. `RETENTION_EXEMPT_TABLES = ["instrument_lifecycle"]` with a doc comment citing
   the SEBI never-delete rule (daily-universe §5/§25). Make the three lists `pub`
   so the meta-guard test can read them.
4. Meta-guard: scan `crates/storage/src/*.rs` for table-name constants
   (`const …TABLE… : &str = "x"`) and assert every one is in
   `HOUR ∪ DAY ∪ EXEMPT`. New audit tables (which all follow that constant
   pattern) are then **automatically** required to declare a retention decision —
   the list can never silently go stale again.

## Edge Cases

- A table listed but not yet created → `DETACH` query returns not-found → already
  handled (logged `warn`, loop continues). No crash.
- `instrument_lifecycle` accidentally added to a sweep list → a dedicated test
  fails the build (SEBI guard).
- A future `*_audit` table added without a retention decision → the meta-guard
  fails the build.
- Shadow tables generated (no single constant) → covered explicitly in the DAY list
  + pinned by a unit test naming all 10.

## Failure Modes

- Wrong table name in a list → `DETACH` no-ops harmlessly (warn). Not data loss.
- Over-sweep (detaching a never-delete table) → prevented by the EXEMPT set + SEBI test.
- The meta-guard itself going stale → it derives the expected set by *scanning source*,
  so it tracks reality, not a hand-maintained copy.

## Test Plan

- `cargo test -p tickvault-storage partition_manager` → list-coverage + SEBI-exclusion tests green.
- `cargo test -p tickvault-storage --test partition_retention_coverage_guard` → meta-guard green.
- `cargo fmt --check`; design-first wall (impl crate `storage` + this APPROVED plan referencing it).

## Rollback

Single-file logic change + one test file. Revert the commit to restore the prior
lists. No schema change, no data migration, no runtime behaviour beyond which
tables the existing detach cycle visits.

## Observability

The existing `detach_old_partitions` already logs `info!(table, detached=count)`
per swept table — now those logs will actually cover the live tables. No new
metric/table. Cold-path maintenance task; no hot-path / O(1) involvement.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item in it are bound by the 15-row 100% guarantee matrix and
the 7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" wording here
means "100% inside the tested envelope, with ratcheted regression coverage" — the
envelope being the list-coverage unit tests + the source-scanning meta-guard. This
is a cold-path storage-retention fix; it carries NO O(1) / zero-tick-loss /
hot-path guarantee, and it does NOT by itself free EBS (the S3-archival of
detached partitions is a separate, AWS-dependent piece, flagged honestly). No
hallucination: stating it "fixes the cost" outright would be false — it fixes the
stale-coverage bug + prevents regression.
