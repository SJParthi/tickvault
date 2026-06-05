# Implementation Plan: Retention sweep must cover the REAL candle tables

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban ("fix everything … no illusion") + hostile-audit CRITICAL
**Crate(s) touched:** `storage`

## Context (verified — a real bug in the just-merged #1022)

#1022 set `DAY_PARTITIONED_TABLES` to 10 `candles_*_shadow` literals copied from
the (stale) rule files. The ACTUAL live candle tables are `candles_1m` …
`candles_1d` — **21 tables, NO `_shadow` suffix** — per `TfIndex::table_name()`
(`crates/trading/src/candles/tf_index.rs:175-197`, the single source of truth:
"there are **no** `_shadow` tables"), enumerated by
`shadow_persistence::candle_table_names()`. Confirmed against the operator's
QuestDB screenshot (`candles_1m`, `candles_10m`, …). So the candle tables — the
dominant disk-growth source — are **still not swept**; #1022 fixed the audit
tables but not the candles. The meta-guard missed it because it only checks
string literals inside `partition_manager.rs` (tautological for a list of
literals); the real names live in a `match` in another crate.

## Plan Items

- [x] Item 1 — Sweep the REAL candle tables from the single source
  - Files: `crates/storage/src/partition_manager.rs`
  - Tests: `partition_manager::tests::test_*`
- [x] Item 2 — Make the meta-guard cross-reference `candle_table_names()`
  - Files: `crates/storage/tests/partition_retention_coverage_guard.rs`

## Design

1. `DAY_PARTITIONED_TABLES` const → the **audit/data** tables only
   (`instrument_fetch_audit`, `instrument_lifecycle_audit`,
   `option_chain_minute_snapshot`, `prev_day_ohlcv`, `cross_verify_1m_audit`).
   Remove all 10 phantom `candles_*_shadow` literals.
2. `detach_old_partitions` gains a third loop over
   `crate::shadow_persistence::candle_table_names()` (DAY granularity, exempt-skip)
   — sweeps all 21 real candle tables **derived from the single source**, so the
   names can never drift from what's actually created/written.
3. Unit tests: assert the DAY const holds the 5 audit/data tables; assert
   `candle_table_names()` returns 21 plain `candles_<TF>` names (no `_shadow`).
4. Meta-guard: import `tickvault_storage::shadow_persistence::candle_table_names`
   and assert (a) `partition_manager.rs` source references `candle_table_names`
   (the candle sweep loop can't be silently deleted) and (b) every candle name is
   a plain `candles_*` (no `_shadow`) so a future rename is caught.

## Edge Cases

- `candles_1d` is created but unwritten (live-feed-purity rule 10) → empty →
  `DETACH` is a harmless no-op. Sweeping all 21 (incl. 1d) is safe and avoids
  special-casing.
- A non-existent table name → `detach_partitions_for_table` returns `Ok(0)` on the
  404 (existing behaviour) — but with the single-source derivation this can't
  happen for candles.
- `instrument_lifecycle` (exempt) is not a candle name → unaffected.

## Failure Modes

- `candle_table_names()` drift → impossible by construction (single source).
- Someone deletes the candle sweep loop → meta-guard source-scan fails the build.
- Vec/alloc concern → N/A: the detach cycle is cold-path (periodic), not hot path.

## Test Plan

- `cargo test -p tickvault-storage partition_manager` → updated coverage + SEBI tests green.
- `cargo test -p tickvault-storage --test partition_retention_coverage_guard` → meta-guard (now candle-aware) green.
- `cargo clippy --workspace -- -D warnings -W clippy::perf` green (the real CI command).

## Rollback

Single-file logic change + meta-guard update. Revert restores #1022's (buggy)
state. No schema/data/runtime change beyond which tables the existing detach
cycle visits.

## Observability

`detach_old_partitions` already logs `info!(table, detached=count)` per table —
now it actually logs the 21 candle tables. No new metric. Cold-path; no hot-path / O(1) involvement.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item in it are bound by the 15-row 100% guarantee matrix and
the 7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" means "100%
inside the tested envelope, with ratcheted regression coverage" — here the
single-source candle derivation + the candle-aware meta-guard. Cold-path
storage-retention fix; NO O(1) / hot-path / zero-tick-loss claim, and it still
does NOT free EBS by itself (S3 archival of detached partitions remains the
separate AWS-dependent piece). No illusion: this corrects the candle-table gap
#1022 left open.
