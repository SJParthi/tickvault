# Implementation Plan: Per-Tick Greeks as Columns in Ticks & Candles

**Status:** APPROVED
**Date:** 2026-03-24
**Approved by:** Parthiban

## Problem

Greeks are currently stored in separate `option_greeks_live` / `pcr_snapshots_live` tables,
requiring JOINs to correlate with ticks and candles. This adds latency and complexity.

**New approach:** Greeks computed per-tick and stored as 6 additional columns directly in the
`ticks` table and `candles_1s` table. All 18 materialized views inherit Greeks via `last()`.
This eliminates JOINs, reduces writes (one ILP call instead of two), and keeps O(1) hot path.

## What Gets Removed

- `option_greeks_live` table DDL, writer, row struct
- `pcr_snapshots_live` table DDL, writer, row struct
- `GreeksAggregator` candle-aligned emission to separate tables
- `run_greeks_aggregator_consumer()` spawn in main.rs

## What Gets Kept

- `option_greeks` table (periodic API snapshot every 60s — cross-verification)
- `pcr_snapshots` table (aggregate PCR — can't be per-instrument column)
- `dhan_option_chain_raw` table (Dhan audit trail)
- `greeks_verification` table (our vs Dhan comparison)
- `GreeksAggregator` core logic (state tracking) — repurposed for inline compute
- `greeks_pipeline.rs` (periodic API pipeline — unchanged)

## Plan Items

- [x] 1. Add 6 Greeks fields to `ParsedTick` struct
  - Files: crates/common/src/tick_types.rs
  - Add `iv: f64`, `delta: f64`, `gamma: f64`, `theta: f64`, `vega: f64`, `rho: f64`
  - Default to `f64::NAN` (QuestDB NULL) for non-F&O ticks
  - Tests: test_parsed_tick_greeks_default_nan, test_parsed_tick_greeks_copy

- [ ] 2. Add 6 Greeks columns to `ticks` table DDL
  - Files: crates/storage/src/tick_persistence.rs
  - Add `iv DOUBLE, delta DOUBLE, gamma DOUBLE, theta DOUBLE, vega DOUBLE, rho DOUBLE`
  - Write Greeks columns in `append_tick()` ILP builder
  - NaN values = QuestDB NULL (no storage cost in columnar format)
  - Tests: test_ticks_ddl_has_greeks_columns, test_append_tick_writes_greeks

- [ ] 3. Add 6 Greeks columns to `candles_1s` base table DDL
  - Files: crates/storage/src/materialized_views.rs
  - Add `iv DOUBLE, delta DOUBLE, gamma DOUBLE, theta DOUBLE, vega DOUBLE, rho DOUBLE`
  - Tests: test_candles_1s_ddl_has_greeks_columns

- [ ] 4. Add `last(iv)`, `last(delta)`, etc. to all 18 materialized views
  - Files: crates/storage/src/materialized_views.rs
  - Modify `build_view_sql()` to include `last(iv) AS iv, last(delta) AS delta, ...`
  - All 18 views inherit Greeks automatically
  - Tests: test_build_view_sql_has_greeks_columns, test_all_views_have_greeks

- [ ] 5. Add Greeks fields to candle struct and candle persistence writer
  - Files: crates/common/src/tick_types.rs, crates/storage/src/candle_persistence.rs
  - Add Greeks to `CandleData` / candle write path
  - Write `last()` Greeks when flushing candles
  - Tests: test_candle_data_has_greeks, test_candle_persistence_writes_greeks

- [ ] 6. Track last Greeks per instrument in candle aggregator
  - Files: crates/core/src/pipeline/candle_aggregator.rs
  - On each F&O tick with valid Greeks: store `(iv, delta, gamma, theta, vega, rho)`
  - On candle close: emit last-seen Greeks as candle's Greeks
  - Equity ticks: NaN Greeks (no computation)
  - Tests: test_candle_aggregator_tracks_greeks, test_candle_close_has_last_greeks

- [ ] 7. Compute Greeks inline in tick processor for F&O ticks
  - Files: crates/core/src/pipeline/tick_processor.rs
  - For NSE_FNO ticks: call `compute_greeks()` with spot (from underlying cache), strike, expiry, LTP
  - For non-F&O ticks: leave Greeks as NaN
  - Needs access to: instrument registry (strike, expiry, option_type), underlying LTP cache
  - Tests: test_tick_processor_computes_greeks_fno, test_tick_processor_nan_greeks_equity

- [ ] 8. Remove `option_greeks_live` table and all related code
  - Files: crates/storage/src/greeks_persistence.rs
  - Remove: `OPTION_GREEKS_LIVE_DDL`, `TABLE_OPTION_GREEKS_LIVE`, `DEDUP_KEY_OPTION_GREEKS_LIVE`
  - Remove: `OptionGreeksLiveRow`, `write_option_greeks_live_row()`
  - Remove: DDL execution in `ensure_greeks_tables()`
  - Tests: test_no_option_greeks_live_table

- [ ] 9. Remove `pcr_snapshots_live` table and all related code
  - Files: crates/storage/src/greeks_persistence.rs
  - Remove: `PCR_SNAPSHOTS_LIVE_DDL`, `TABLE_PCR_SNAPSHOTS_LIVE`, `DEDUP_KEY_PCR_SNAPSHOTS_LIVE`
  - Remove: `PcrSnapshotLiveRow`, `write_pcr_snapshot_live_row()`
  - Remove: DDL execution in `ensure_greeks_tables()`
  - Tests: test_no_pcr_snapshots_live_table

- [ ] 10. Remove GreeksAggregator candle-aligned consumer from main.rs
  - Files: crates/app/src/main.rs
  - Remove: `run_greeks_aggregator_consumer()` spawn
  - Keep: periodic `run_greeks_pipeline()` spawn (API cross-verification)
  - Tests: compile check (dead code elimination)

- [ ] 11. Update GreeksAggregator — repurpose for inline tick computation
  - Files: crates/trading/src/greeks/aggregator.rs
  - Remove: `GreeksEmission`, `emit_snapshot()`, candle-close emission logic
  - Keep: underlying LTP cache, option state tracking
  - Add: `compute_for_tick(tick) -> (iv, delta, gamma, theta, vega, rho)` method
  - Tests: test_compute_for_tick_returns_greeks, test_compute_for_tick_no_spot_returns_nan

- [ ] 12. Update all existing tests for schema changes
  - Files: crates/storage/tests/*, crates/core/tests/*, crates/trading/tests/*
  - Update DDL assertion tests for new columns
  - Update tick/candle struct tests for Greeks fields
  - Update integration tests that build ParsedTick/CandleData
  - Tests: all existing tests must pass with new schema

## Architecture (New)

```
WebSocket (Full mode, 162 bytes)
    │
    ▼
Tick Processor
    │
    ├─ Parse binary → ParsedTick
    │
    ├─ IF NSE_FNO:
    │     spot = underlying_ltp_cache[underlying_security_id]
    │     greeks = compute_greeks(spot, strike, T, r, option_ltp)
    │     tick.iv = greeks.iv
    │     tick.delta = greeks.delta (... etc)
    │  ELSE:
    │     tick.iv = NaN (... etc)
    │
    ├─► Tick Persistence → ticks table (WITH Greeks columns)
    │
    ├─► Broadcast → Candle Aggregator
    │                   │
    │                   ├─ Track last Greeks per instrument
    │                   ├─ On candle close: candle.iv = last_greeks.iv
    │                   └─► candles_1s table (WITH Greeks columns)
    │                           │
    │                           └─► QuestDB Materialized Views
    │                               candles_5s..1M all get last(iv), last(delta)...
    │
    └─► [Existing] Greeks Pipeline (REST API, every 60s, audit only)
```

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | F&O tick arrives with spot available | Greeks computed, 6 columns populated in tick |
| 2 | F&O tick arrives, no spot yet | Greeks = NaN (no underlying LTP cache hit) |
| 3 | Equity tick arrives | Greeks = NaN (not F&O) |
| 4 | IDX_I tick arrives | Greeks = NaN, but updates underlying LTP cache |
| 5 | 1s candle closes with 10 F&O ticks | Candle Greeks = last tick's Greeks |
| 6 | 1s candle closes with 0 F&O ticks | Candle Greeks = NaN |
| 7 | Materialized view candles_5m | Gets `last(iv)` from candles_1s = last 1s candle's Greeks |
| 8 | Query: "What was delta at 09:15?" | Direct column read from candles_1m, no JOIN |
