# Implementation Plan: Align market_open config to 09:00 + clarify dual-window docs

**Status:** DRAFT
**Date:** 2026-03-18
**Approved by:** pending

## Context

Two distinct windows exist in the system:

1. **Data storage window:** 09:00 → 15:30 exclusive (ticks + candles persisted from 09:00)
2. **Cross-verification window:** 09:15 → 15:30 exclusive (historical API only has data from 09:15)

The storage pipeline already works correctly:
- `TICK_PERSIST_START_SECS_OF_DAY_IST = 32400` (09:00) ✓
- `data_collection_start = "09:00:00"` (WebSocket connects at 09:00) ✓
- Candle persist window = [09:00, 15:30) ✓

The cross-verification already works correctly:
- `CANDLES_PER_TRADING_DAY = 375` (09:15→15:29) ✓
- `MARKET_OPEN_TIME_IST = "09:15:00"` (Dhan historical API start) ✓

**What's WRONG:** `config/base.toml` says `market_open_time = "09:15:00"` and all test defaults match — this is misleading since our system collects data from 09:00. Doc comments in several files also conflate the two windows.

## Plan Items

- [ ] **Item 1: Update config/base.toml — market_open_time to 09:00**
  - `market_open_time`: "09:15:00" → "09:00:00"
  - Files: `config/base.toml`

- [ ] **Item 2: Update config.rs — doc comment and test default**
  - Doc comment on `market_open_time` field: "09:15:00" → "09:00:00"
  - `make_valid_config()` default: "09:15:00" → "09:00:00"
  - Files: `crates/common/src/config.rs`

- [ ] **Item 3: Update trading_calendar.rs — test default**
  - `make_test_config()`: "09:15:00" → "09:00:00"
  - Files: `crates/common/src/trading_calendar.rs`

- [ ] **Item 4: Update test defaults in config_round_trip.rs, pipeline_smoke.rs**
  - `config_round_trip.rs`: TOML string and struct default "09:15:00" → "09:00:00"
  - `pipeline_smoke.rs`: test default "09:15:00" → "09:00:00"
  - Files: `crates/common/tests/config_round_trip.rs`, `crates/app/tests/pipeline_smoke.rs`

- [ ] **Item 5: Clarify constants.rs doc comments — dual window explanation**
  - `MARKET_OPEN_TIME_IST`: add doc explaining this is for Dhan historical API fetch start (09:15), NOT data collection start (09:00)
  - `CANDLES_PER_TRADING_DAY`: clarify this is for cross-verification window (09:15→15:29 = 375), NOT total stored candles
  - Files: `crates/common/src/constants.rs`

- [ ] **Item 6: Clarify historical/mod.rs doc comment — dual window**
  - Update to explain: data storage from 09:00, historical API from 09:15, cross-verify uses 09:15–15:29
  - Files: `crates/core/src/historical/mod.rs`

- [ ] **Item 7: Clarify cross_verify.rs doc comment**
  - Update "95% of 375 = 356" comment to explain WHY 375 (cross-verify window = 09:15–15:29)
  - Files: `crates/core/src/historical/cross_verify.rs`

- [ ] **Item 8: Clarify tick_persistence.rs test comments**
  - Comment "09:15 - 15:30" → "09:00 - 15:30 (storage)" and note cross-verify is 09:15–15:30
  - Files: `crates/storage/src/tick_persistence.rs`

- [ ] **Item 9: Clarify candle_persistence.rs doc comment**
  - "09:15" → "09:00" in data storage context
  - Files: `crates/storage/src/candle_persistence.rs`

- [ ] **Item 10: Build & test — verify all pass**
  - `cargo build --workspace`
  - `cargo test --workspace`
  - Files: N/A
  - Tests: full workspace

## NOT changed (intentional)

- `MARKET_OPEN_TIME_IST = "09:15:00"` — stays, this is for Dhan historical API (data from 09:15)
- `CANDLES_PER_TRADING_DAY = 375` — stays, this is for cross-verification (09:15→15:29)
- `TICK_PERSIST_START_SECS_OF_DAY_IST = 32400` — already 09:00 ✓
- `data_collection_start = "09:00:00"` — already 09:00 ✓
- `MARKET_CLOSE_TIME_IST_EXCLUSIVE = "15:30:00"` — unchanged ✓
- `MARKET_LAST_CANDLE_START_IST = "15:29:00"` — unchanged ✓
- `build_window_end: "09:15:00"` in route_coverage/api_smoke — instrument build window, NOT market open
- Historical fetcher `from_date` — uses `MARKET_OPEN_TIME_IST` ("09:15:00") correctly
- candle_fetcher.rs tests — test Dhan API behavior (09:15 start), stay as-is

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Live WebSocket ticks from 09:00 | Stored in QuestDB (persist window starts 09:00) |
| 2 | Live candles 09:00–09:14 | Stored in QuestDB (within [09:00, 15:30) window) |
| 3 | Historical fetch from Dhan API | Requests from 09:15 (MARKET_OPEN_TIME_IST), Dhan has data from 09:15 |
| 4 | Cross-verification historical vs live | Compares 09:15–15:29 only (375 candles), ignores pre-market live candles |
| 5 | Config validation | market_open_time "09:00:00" < market_close_time "15:30:00" ✓ |
