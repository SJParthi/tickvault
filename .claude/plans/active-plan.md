# Implementation Plan: Fix Market Dashboard — Enrich Movers with Instrument Metadata

**Status:** APPROVED
**Date:** 2026-04-07
**Approved by:** Parthiban

## Summary

Fix the market dashboard so it matches Dhan's web.dhan.co precisely. Currently all movers
data is written to QuestDB with empty symbol names and zero metadata because the
InstrumentRegistry is never passed to the tick processor's persist functions.

## Root Cause

`persist_stock_movers_snapshot()` and `persist_option_movers_snapshot()` in tick_processor.rs
write empty strings for symbol/contract_name/underlying/option_type/expiry and 0.0 for
prev_close/strike/spot_price because:
1. `MoverEntry` doesn't carry `prev_close` (tracker has it but doesn't pass it through)
2. `InstrumentRegistry` is not available in the tick processor
3. Dashboard HTML has missing categories and columns vs Dhan web.dhan.co

## Plan Items

- [ ] Item 1: Add prev_close to MoverEntry, populate in compute_snapshot
  - Files: crates/core/src/pipeline/top_movers.rs
  - Tests: existing top_movers tests

- [ ] Item 2: Pass InstrumentRegistry to run_tick_processor, enrich stock movers
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Change: Add `Option<Arc<InstrumentRegistry>>` param, lookup symbol + use prev_close
  - Tests: existing tick_processor tests

- [ ] Item 3: Enrich option movers with contract_name, underlying, option_type, strike, expiry
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Change: Lookup security_id in registry for all option metadata
  - Tests: existing option_movers tests

- [ ] Item 4: Update main.rs to pass registry clone to run_tick_processor
  - Files: crates/app/src/main.rs
  - Change: Clone registry from subscription_plan, pass to both fast-boot and slow-boot paths
  - Tests: compilation + boot

- [ ] Item 5: Update dashboard HTML to match Dhan web.dhan.co layout precisely
  - Files: crates/api/static/market-dashboard.html
  - Change: Add Gainers/Losers toggle, Value column, expiry filter for options, proper formatting
  - Tests: visual verification

- [ ] Item 6: Build, clippy, test
  - Files: n/a
  - Tests: cargo build + cargo test --workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Stock movers persist | symbol field populated with real name (e.g., "RELIANCE") |
| 2 | Option movers persist | contract_name like "NIFTY 28 APR 23000 CALL", underlying, strike, expiry filled |
| 3 | Dashboard stocks tab | Shows stock names, LTP, change, change%, volume |
| 4 | Dashboard options tab | Shows contract names, spot, LTP, OI, volume |
| 5 | Dashboard index tab | Shows all NSE indices with OHLC |
