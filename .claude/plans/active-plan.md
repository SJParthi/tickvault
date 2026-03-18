# Implementation Plan: Align market_open to 09:00 across all constants, config, and tests

**Status:** DRAFT
**Date:** 2026-03-18
**Approved by:** pending

## Context

Live tick persistence and candle persistence windows already start at 09:00 IST (`TICK_PERSIST_START_SECS_OF_DAY_IST = 32400`). However, `MARKET_OPEN_TIME_IST`, `CANDLES_PER_TRADING_DAY`, `config/base.toml`, and all test defaults still reference 09:15. This means:
- Historical candle fetcher requests data from 09:15 (misses pre-market)
- `CANDLES_PER_TRADING_DAY = 375` (should be 390 for 09:00–15:29)
- Cross-verification uses 375 as baseline

## Plan Items

- [ ] **Item 1: Update constants.rs — MARKET_OPEN_TIME_IST and CANDLES_PER_TRADING_DAY**
  - `MARKET_OPEN_TIME_IST`: "09:15:00" → "09:00:00"
  - `CANDLES_PER_TRADING_DAY`: 375 → 390 (09:00 to 15:29 = 390 minutes)
  - Doc comments updated
  - Test `test_market_open_time` assertion: "09:15:00" → "09:00:00"
  - Files: `crates/common/src/constants.rs`
  - Tests: `test_market_open_time`

- [ ] **Item 2: Update config/base.toml**
  - `market_open_time`: "09:15:00" → "09:00:00"
  - Files: `config/base.toml`

- [ ] **Item 3: Update config.rs — doc comment and test default**
  - Doc comment on `market_open_time` field: "09:15:00" → "09:00:00"
  - `make_valid_config()` default: "09:15:00" → "09:00:00"
  - Files: `crates/common/src/config.rs`
  - Tests: existing config validation tests pass

- [ ] **Item 4: Update trading_calendar.rs — test default**
  - `make_test_config()`: "09:15:00" → "09:00:00"
  - Files: `crates/common/src/trading_calendar.rs`

- [ ] **Item 5: Update historical module doc comment**
  - Line 15: "09:15:00 IST to 15:29:00 IST (375" → "09:00:00 IST to 15:29:00 IST (390"
  - Files: `crates/core/src/historical/mod.rs`

- [ ] **Item 6: Update candle_fetcher.rs — test values and comments**
  - Test `test_intraday_request_serialization`: from_date "09:15:00" → "09:00:00"
  - Test `test_dhan_rest_api_returns_utc_epoch_market_open`: update to 09:00 IST = 03:30 UTC
  - Comment on line 252: "09:15" → "09:00"
  - Files: `crates/core/src/historical/candle_fetcher.rs`
  - Tests: `test_intraday_request_serialization`, `test_dhan_rest_api_returns_utc_epoch_market_open`

- [ ] **Item 7: Update cross_verify.rs — doc comment**
  - Comment "95% of 375 = 356" → "95% of 390 = 370"
  - Files: `crates/core/src/historical/cross_verify.rs`

- [ ] **Item 8: Update tick_persistence.rs tests — comments and range**
  - Comment "09:15 - 15:30" → "09:00 - 15:30"
  - Test `test_dhan_epoch_market_open_0915_ist`: rename and update to 09:00 IST
  - Test `test_dhan_epoch_all_market_hours_display_correct_ist`: start at (9, 0, 0), range 540..=930
  - Files: `crates/storage/src/tick_persistence.rs`
  - Tests: `test_dhan_epoch_market_open_0900_ist`, `test_dhan_epoch_all_market_hours_display_correct_ist`

- [ ] **Item 9: Update candle_persistence.rs — doc comment**
  - "09:15" → "09:00" in comment
  - Files: `crates/storage/src/candle_persistence.rs`

- [ ] **Item 10: Update test defaults in config_round_trip.rs, pipeline_smoke.rs**
  - `config_round_trip.rs`: TOML string and struct default "09:15:00" → "09:00:00"
  - `pipeline_smoke.rs`: test default "09:15:00" → "09:00:00"
  - Files: `crates/common/tests/config_round_trip.rs`, `crates/app/tests/pipeline_smoke.rs`

- [ ] **Item 11: Build & test — verify all pass**
  - `cargo build --workspace`
  - `cargo test --workspace`
  - Files: N/A
  - Tests: full workspace

## NOT changed (intentional)

- `build_window_end: "09:15:00"` in `route_coverage.rs` / `api_smoke.rs` — this is instrument build window, NOT market open
- `TICK_PERSIST_START_SECS_OF_DAY_IST = 32400` — already 09:00 ✓
- `data_collection_start = "09:00:00"` — already 09:00 ✓
- `MARKET_CLOSE_TIME_IST_EXCLUSIVE = "15:30:00"` — unchanged ✓
- `MARKET_LAST_CANDLE_START_IST = "15:29:00"` — unchanged ✓

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Historical fetcher requests from Dhan | fromDate starts at 09:00:00, gets pre-market data if available |
| 2 | Cross-verification with 390 candle baseline | 95% of 390 = 370 minimum; historical data with 375 candles passes |
| 3 | Live candle persistence | Candles from 09:00-09:14 persist (already working, now consistent with constants) |
| 4 | Config validation | market_open_time "09:00:00" < market_close_time "15:30:00" ✓ |
