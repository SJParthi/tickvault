# Implementation Plan: Top Movers Persistence + Options Movers + Depth Selection + Warmup

**Status:** DRAFT
**Date:** 2026-03-29
**Approved by:** pending

## Decisions Made

- **Storage interval:** 1-minute snapshots (09:15 inclusive — 15:30 exclusive)
- **Options movers:** All 7 categories (Highest OI, OI Gainers, OI Losers, Top Volume, Top Value, Price Gainers, Price Losers)
- **Stock movers:** 3 categories (Gainers, Losers, Most Active)
- **Options filter:** Current month expiry + All types (default). Configurable.
- **Market depth:** Major F&O indices (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY) for 20-level depth
- **Tables:** Separate `stock_movers` + `option_movers` (different schemas)

## Plan Items

### Phase A: Stock Top Movers Persistence (Day 1)

- [ ] Item 1: Create `stock_movers` QuestDB table DDL + persistence writer
  - Files: crates/storage/src/movers_persistence.rs (NEW), crates/storage/src/lib.rs
  - Tests: test_stock_movers_table_ddl, test_append_stock_mover, test_flush_batch

- [ ] Item 2: Add 1-minute snapshot persistence to tick processor
  - Files: crates/core/src/pipeline/tick_processor.rs, crates/core/src/pipeline/top_movers.rs
  - Tests: test_snapshot_at_minute_boundary, test_no_snapshot_outside_hours

- [ ] Item 3: Enrich TopMoversSnapshot with symbol name from instrument registry
  - Files: crates/core/src/pipeline/top_movers.rs
  - Tests: test_mover_entry_with_symbol

### Phase B: Options Movers Computation + Persistence (Day 2-3)

- [ ] Item 4: Create OptionMoversTracker with all 7 categories
  - Files: crates/core/src/pipeline/option_movers.rs (NEW), crates/core/src/pipeline/mod.rs
  - Categories: Highest OI, OI Gainers, OI Losers, Top Volume, Top Value, Price Gainers, Price Losers
  - Tests: test_oi_change_calculation, test_all_7_categories_sorted, test_expiry_filter, test_nan_filtered

- [ ] Item 5: Create `option_movers` QuestDB table DDL + persistence writer
  - Files: crates/storage/src/movers_persistence.rs (extend)
  - Tests: test_option_movers_table_ddl, test_append_option_mover

- [ ] Item 6: Wire OptionMoversTracker into tick processor + 1-minute persistence
  - Files: crates/core/src/pipeline/tick_processor.rs, crates/app/src/main.rs
  - Tests: test_option_movers_snapshot_persisted, test_outside_hours_skipped

- [ ] Item 7: Add API endpoints for options movers
  - Files: crates/api/src/handlers/option_movers.rs (NEW), crates/api/src/lib.rs
  - Tests: test_option_movers_endpoint_200, test_option_movers_categories

### Phase C: Market Depth Instrument Selection (Day 4)

- [ ] Item 8: Add depth subscription config + selection logic
  - Files: crates/core/src/instrument/subscription_planner.rs, crates/common/src/config.rs
  - Tests: test_depth_instrument_selection, test_depth_config_defaults

- [ ] Item 9: Wire 20-level depth WS connection in boot sequence
  - Files: crates/app/src/main.rs, crates/core/src/websocket/connection_pool.rs
  - Tests: test_depth_connection_created, test_depth_subscription_uses_code_23

### Phase D: Indicator Warmup from Historical (Day 5)

- [ ] Item 10: Load historical candles into indicator engine before market open
  - Files: crates/trading/src/indicator/engine.rs, crates/app/src/trading_pipeline.rs
  - Tests: test_warmup_from_historical, test_warmup_skips_if_no_data

### Phase E: Auto-Reconciliation + Phase Doc Update (Day 6)

- [ ] Item 11: Schedule automatic reconciliation every 30s during market hours
  - Files: crates/app/src/trading_pipeline.rs
  - Tests: test_reconciliation_scheduled, test_reconciliation_skipped_outside_hours

- [ ] Item 12: Update phase doc checkboxes
  - Files: docs/phases/phase-1-live-trading.md

### Phase F: Grafana Dashboards (Day 6)

- [ ] Item 13: Stock movers Grafana panels (Gainers, Losers, Most Active tables)
  - Files: deploy/docker/grafana/dashboards/market-data.json

- [ ] Item 14: Options movers Grafana panels (Highest OI, OI Gainers/Losers, Top Volume)
  - Files: deploy/docker/grafana/dashboards/market-data.json

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot on trading day at 09:00 | Movers trackers initialize empty, no snapshots until 09:15 |
| 2 | First tick at 09:15:00 | Movers start tracking, first snapshot at 09:16:00 |
| 3 | Every minute 09:16-15:29 | Snapshot persisted for both stock + option movers |
| 4 | Tick at 15:30:00 | No snapshot persisted (exclusive boundary) |
| 5 | Non-trading day boot | Movers trackers initialize but no snapshots persisted |
| 6 | NaN/Infinity LTP in option | Filtered, not included in rankings |
| 7 | Zero OI option | Valid for price movers, excluded from OI rankings |
| 8 | QuestDB down during persist | Error logged, next snapshot retried |
| 9 | WebSocket reconnect | Movers state survives in-memory, gap in snapshots |
| 10 | 20-level depth subscription | Only configured indices, separate WS connection |
