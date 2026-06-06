# Implementation Plan: NTM Map-B — boot population of the full per-index mapping

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — "yes do this … full per-index mapping … I need guarantee and
assurance" + uploaded the real `ind_niftytotalmarket_list.csv` (parser validated: 748 cash-equity,
100% ISIN). Map-A (`index_constituency` table) merged #1043.
**Crate(s) touched:** `app` (new `index_constituency_boot.rs`; `lib.rs` registration;
`main.rs` one spawn call site). Feature: `daily_universe_fetcher`.

## Context

§31 item 2: persist the FULL index→constituents mapping for all ~46 tracked indices so "which
indices is stock X in / which stocks in index Y" is queryable (Map-A table). Map-B is the boot
population. It is a **separate, decoupled, map-only** step — the live NTM-union subscription path is
**byte-for-byte untouched** (operator's explicit "don't change existing logic" concern, asked twice).

## Design

- New `persist_index_constituency_mapping(questdb, today, ts_nanos, dry_run)` — degrade-safe async,
  **spawned fire-and-forget** from `cold_build_daily_universe` AFTER the universe build. Steps:
  1. `build_constituency_map(INDEX_CONSTITUENCY_SLUGS)` — full map (all ~46 indices), map-only.
  2. Independently re-fetch + `parse_detailed_csv` the Dhan master (so the subscription path is not
     touched — it keeps its own NTM-only fetch).
  3. `resolve_constituents(full_map, rows)` — ISIN-primary, the same resolver Map-A/#4 use.
  4. PURE `build_index_constituency_rows(full_map, resolved)` → one `OwnedConstituencyRow` per
     `(index, stock)` (unresolved/non-numeric SID skipped).
  5. `ensure_index_constituency_table` + `append_index_constituency_rows` (Map-A writer, idempotent
     DEDUP UPSERT).
- The pure row-builder is unit-tested; the network/QuestDB shell is TEST-EXEMPT (its deps are tested
  in their own crates + the real-CSV parse was runtime-validated this session).

## Edge Cases

- niftyindices down / empty map → `warn!` + return (subscription unaffected).
- Dhan CSV fetch/parse fail → `warn!` + return.
- resolve dangling > 0.5% → resolver `Err` → `warn!` + return.
- A constituent symbol not in the resolved set (dangling) → skipped (resolver already counted it).
- Non-numeric `security_id` → skipped defensively.
- Same stock in N indices → N rows (per-index membership), deduped by Map-A's `(ts, index_name,
  security_id, exchange_segment)` key.

## Failure Modes

- Every failure path is `warn!` + early return — NEVER blocks boot, NEVER touches the live feed.
- Spawned task panic → isolated (tokio task), does not affect boot.
- Persist `Err` → `warn!`; next boot re-runs (idempotent DEDUP).

## Test Plan

`cargo test -p tickvault-app` (full crate — the lesson). Unit tests on the pure builder:
per-index membership (RELIANCE×2 + INFY×1), unresolved skipped, non-numeric SID skipped, via_isin
preserved, empty map empty. Module compiles + the full app crate suite (19 bins) green; pub-fn-test
(121 stable), banned-pattern, pub-fn-wiring all green. Real-CSV parse was runtime-validated
(748 constituents, missing_isin=0) this session.

## Rollback

New module + 1 `lib.rs` line + 1 spawn call site. `git revert` is clean — the spawn just stops
running; the live subscription + universe build are unaffected (they never depended on it). No
migration (Map-A table is `CREATE IF NOT EXISTS`).

## Observability

`info!` on success (indices, rows, unresolved counts); `warn!` on each degrade path. The
`index_constituency` table (Map-A) is the queryable surface. No new error code (map-only degrade is
a `warn!`, not a trading-critical failure).

## Plan Items

- [x] Item 1 — `index_constituency_boot.rs`: pure `build_index_constituency_rows` + degrade-safe
  `persist_index_constituency_mapping`
  - Files: `crates/app/src/index_constituency_boot.rs`, `crates/app/src/lib.rs`
  - Tests: `build_index_constituency_rows_maps_each_index_membership`,
    `build_rows_skips_unresolved_constituent`, `build_rows_skips_non_numeric_security_id`,
    `build_rows_preserves_via_isin_flag`, `build_rows_empty_map_is_empty`
- [x] Item 2 — wire the fire-and-forget spawn into `cold_build_daily_universe`
  - Files: `crates/app/src/main.rs`
  - Tests: build_index_constituency_rows_maps_each_index_membership
    (spawned path's pure core; the spawn itself is pub-fn-wiring confirmed, no dedicated unit test)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | healthy boot | all ~46 indices' constituents resolved + persisted; subscription untouched |
| 2 | niftyindices down | `warn!` + skip; live feed + universe unaffected |
| 3 | stock in 5 indices | 5 rows (one per index), queryable both directions |
| 4 | revert | spawn stops; zero effect on subscription/universe |
