# Implementation Plan: NTM Map-A — index_constituency persistence layer

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — "yes do this … I need guarantee and assurance" + "full per-index
mapping" (AskUserQuestion 2026-06-06). Split Map-A (this PR, persistence) → Map-B (boot wiring).
**Crate(s) touched:** `storage` (new `index_constituency_persistence.rs` + `lib.rs` registration).
Feature: `daily_universe_fetcher`. **No boot change** — self-contained, fully tested first.

## Context

§31 item 2 + operator verification: the full index→constituents mapping for all ~46 tracked
indices must be **persisted + queryable both directions** ("which indices is stock X in / which
stocks in index Y"). No `index_constituency` table existed. Map-A ships the persistence layer;
Map-B wires the boot fetch (all 46 lists → resolve per-index → write here), subscription staying
NTM-union-only.

## Design

- New table `index_constituency` — one row per `(trading_date, index_name, security_id,
  exchange_segment)`. Columns: `ts` (designated = IST trading-date midnight), `index_name` SYMBOL,
  `security_id` LONG, `exchange_segment` SYMBOL, `symbol_name` SYMBOL, `isin` SYMBOL, `via_isin`
  BOOLEAN, `source` SYMBOL, `dry_run` BOOLEAN.
- **DEDUP UPSERT KEYS `(ts, index_name, security_id, exchange_segment)`** — `ts` (designated, =
  trading date) keeps daily rebalance history; `index_name` distinguishes the same stock across
  its many indices; `security_id`+`exchange_segment` is I-P1-11 composite identity.
- `ensure_index_constituency_table` (idempotent DDL), `build_index_constituency_ilp_row` (pure ILP
  builder), `append_index_constituency_rows` (bulk ILP on `spawn_blocking`). Mirrors the canonical
  `instrument_lifecycle_persistence` template.

## Edge Cases

- Same stock in many indices → distinct rows (index_name in the key), not collapsed.
- Empty `isin` (symbol-fallback) → SYMBOL skipped (→ NULL, not empty ILP symbol); `via_isin=false`.
- Re-run same day → DEDUP UPSERT in place (idempotent); next day → new partition rows (history).
- Large row count (~thousands of (index,stock) pairs) → ILP bulk (not the `/exec` URL door).
- Cross-segment `security_id` reuse → segment in the key prevents collision (I-P1-11).

## Failure Modes

- DDL non-2xx / transient → `warn!` (mirrors sibling ensure_*); the bulk write surfaces a hard
  `Err` the Map-B caller logs + the next idempotent boot re-runs.
- ILP connect/flush failure → `Err` propagated; idempotent re-run safe (DEDUP).

## Test Plan

`cargo test -p tickvault-storage --features daily_universe_fetcher`:
- `test_table_name_constant_is_stable`, `test_dedup_key_includes_segment_and_index_and_designated_ts`,
  `test_ddl_columns_constant_matches_schema`, `test_build_ilp_row_symbols_before_fields_and_contains_values`,
  `test_build_ilp_row_skips_empty_isin`.
- `dedup_segment_meta_guard` (workspace meta-guard) — confirms the new DEDUP key includes segment.
- pure ILP builder unit-tested via `buffer.as_bytes()`; network paths TEST-EXEMPT (Map-B boot
  integration exercises the flush).

## Rollback

Pure-additive new module + 1 `lib.rs` line; nothing consumes it yet (Map-B will). `git revert` is
clean — no migration, no caller. The table is `CREATE IF NOT EXISTS` so even a half-applied deploy
self-heals.

## Observability

The table itself IS the queryable observability (both-direction SQL). Map-B adds the boot write +
counts + degrade alert. No new error code (write failure is a propagated `Err` + `warn!`).

## Plan Items

- [x] Item 1 — `index_constituency_persistence.rs`: table const, composite DEDUP key, DDL, pure ILP
  builder, bulk writer
  - Files: `crates/storage/src/index_constituency_persistence.rs`, `crates/storage/src/lib.rs`
  - Tests: `test_table_name_constant_is_stable`,
    `test_dedup_key_includes_segment_and_index_and_designated_ts`,
    `test_ddl_columns_constant_matches_schema`,
    `test_build_ilp_row_symbols_before_fields_and_contains_values`,
    `test_build_ilp_row_skips_empty_isin`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | RELIANCE in NIFTY 50 + NIFTY BANK + NTM | 3 distinct rows (one per index) |
| 2 | constituent resolved via symbol fallback (no ISIN) | row with isin NULL, via_isin=false |
| 3 | same-day re-run | DEDUP UPSERT in place (no dup) |
| 4 | next trading day | new partition rows (rebalance history kept) |
