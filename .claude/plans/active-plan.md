# PR-4 HOTFIX — daily-universe runtime bugs (boot blocked + missing schema)

**Status:** APPROVED (operator: "fix and implement everything entirely")
**Date:** 2026-05-28
**Discovered:** live local run 16:16 IST — boot HANGS at Step 6c; instrument_lifecycle table 400; no 250 SIDs; no ticks; no % changes.
**Root-caused (all confirmed against code + docs/dhan-ref/09-instrument-master.md):**

## Bug A — `instrument_lifecycle` DDL → 400 Bad Request (table never created)
- **Cause:** `crates/storage/src/instrument_lifecycle_persistence.rs:381` uses `timestamp(ts) PARTITION BY NONE WAL ... DEDUP UPSERT KEYS(...)`. QuestDB **rejects WAL+DEDUP on a non-partitioned table**. Audit tables work (they use `PARTITION BY DAY WAL`).
- **Fix:** change to `PARTITION BY DAY WAL`. Constant `ts` (epoch-0, I-P1-08) → all rows land in the 1970-01-01 partition; DEDUP on `(ts, security_id, exchange_segment)` still works. Verify the reconcile INSERT writes a CONSTANT ts (else daily partitions + no cross-day dedup).
- **Ratchet/doc updates:** test at line ~1053 asserts `PARTITION BY NONE WAL` → flip to DAY; module doc lines 61-62, 340-343.

## Bug B — boot BLOCKED (INSTR-FETCH-03), no 250 SIDs (SYSTEMIC)
- **Cause:** code matches CSV `SEGMENT` column against `"NSE_EQ"`/`"IDX_I"`, but Dhan Detailed CSV `SEGMENT` is **single-char** (`E`=Equity, `I`=Index, `D`=Derivatives, `C`, `M`) per `docs/dhan-ref/09-instrument-master.md:37`. So `nse_eq_lookup` is empty → all FUTSTK/OPTSTK underlyings "dangling" → >0.5% → reject → infinite retry.
- **Fix (parse-time derivation — cleanest, fixes the whole chain):** in `crates/core/src/instrument/csv_parser.rs` row construction (~line 345), derive canonical `exchange_segment` from `(exch_id, segment_char)` and store THAT in `row.segment`:
  - `seg="I"` → `IDX_I` (both NSE+BSE indices)
  - `exch=NSE,seg=E` → `NSE_EQ`; `exch=NSE,seg=D` → `NSE_FNO`
  - `exch=BSE,seg=E` → `BSE_EQ`; `exch=BSE,seg=D` → `BSE_FNO`
  - `seg=C` → `{NSE,BSE}_CURRENCY`; `seg=M` → `MCX_COMM`
- **Then unchanged-and-correct:** `fno_underlying_extractor.rs:53/128` (NSE_EQ), `index_extractor.rs` (IDX_I filter), `daily_universe.rs`, lifecycle `exchange_segment` column.
- **New test:** parser test feeding the REAL single-char codes (`E`/`I`/`D`) asserting `row.segment` derives `NSE_EQ`/`IDX_I`/`NSE_FNO`. This is the regression guard the merged PRs lacked (all unit tests used synthetic `segment:"NSE_EQ"`).
- **Audit `index_extractor.rs`** for the same `"IDX_I"` vs `"I"` mismatch (the index half of the 250 SIDs).

## Concern C — percentage-change columns removed (operator expects them)
- **Finding:** Engine-B candle tables = 10 cols, **NO `*_pct_from_prev_day`** (`shadow_persistence.rs:17,35`). The 3 pct columns were dropped in the Engine-B rewrite.
- **Decision needed + fix:** re-add `close_pct_from_prev_day`, `oi_pct_from_prev_day`, `volume_pct_from_prev_day` (DOUBLE) to the candle DDL + restore seal-time pct-stamping (prev_day cache → seal), OR confirm % is computed in-memory/elsewhere. Operator wants visible % changes.

## Table/column create-update audit (operator: "all tables/columns should be created/updated")
- Verify every merged-PR table actually CREATEs at runtime (not just unit-tested): instrument_lifecycle (A), lifecycle_audit ✓, fetch_audit ✓, 21 candle TFs ✓, ticks ✓, option_chain_minute_snapshot ✓.
- Verify ALTER ADD COLUMN IF NOT EXISTS self-heal for any new columns.

## Why this regressed past CI
All daily-universe unit/source-scan tests used synthetic rows + never hit a live QuestDB or the real CSV. Add: (1) parser real-segment test, (2) a boot-integration smoke that runs the DDLs against a live QuestDB in CI.

## Sequence
PR-4a: Bug A (DDL) + Bug B (segment derivation) + tests → unblocks boot + builds 250 SIDs. PR-4b: Concern C (% columns) after operator confirms intent.
