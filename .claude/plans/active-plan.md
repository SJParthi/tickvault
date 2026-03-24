# Implementation Plan: Tick-Driven Greeks Pipeline with Exchange Timestamp Alignment

**Status:** DRAFT
**Date:** 2026-03-24
**Approved by:** pending

## Problem

Greeks/PCR snapshots currently use `Utc::now()` as timestamp (system clock from REST API fetch time).
This cannot be mapped to ticks/candles which use `exchange_timestamp` (NSE clock).
Need: Greeks.ts = Tick.exchange_timestamp so all data aligns on the same time axis.

## Approach

Add a **tick-driven Greeks consumer** on the existing broadcast channel. When an F&O option
tick arrives via WebSocket (Full mode, 162 bytes), compute Greeks using:
- Option LTP → from the tick itself (`parsed_tick.last_traded_price`)
- Spot price → from the underlying's latest tick (cached in O(1) map)
- Strike/Expiry/OptionType → from instrument registry (O(1) lookup)
- Timestamp → from `parsed_tick.exchange_timestamp` (NSE clock, IST epoch secs)

Result: `option_greeks_live.ts` = tick's exchange timestamp = candle's ts = exact alignment.

## Plan Items

- [ ] 1. Add underlying LTP cache to track index/equity spot prices from ticks
  - Files: crates/core/src/pipeline/greeks_aggregator.rs (NEW)
  - Pattern: `HashMap<u32, f32>` mapping underlying security_id → latest LTP
  - Updated on every IDX_I/NSE_EQ tick; read when computing F&O Greeks
  - Tests: test_underlying_ltp_cache_update, test_underlying_ltp_cache_miss_skips_greeks

- [ ] 2. Add underlying security_id reverse lookup (symbol → security_id)
  - Files: crates/common/src/instrument_registry.rs
  - Add `fn get_underlying_security_id(&self, symbol: &str) -> Option<u32>` method
  - Build reverse map at registry construction time (cold path, O(N) once)
  - Tests: test_underlying_reverse_lookup, test_underlying_reverse_lookup_missing

- [ ] 3. Create `GreeksAggregator` — tick-driven Greeks computation engine
  - Files: crates/core/src/pipeline/greeks_aggregator.rs (NEW)
  - On each F&O tick: registry lookup → get underlying LTP → compute Greeks → emit
  - Uses `exchange_timestamp` for time-to-expiry AND as the output row timestamp
  - Throttle: max 1 Greeks update per security_id per second (avoid flooding QuestDB)
  - O(1) per tick: HashMap lookup + BS formula (no allocation)
  - Tests: test_greeks_aggregator_computes_on_fno_tick, test_greeks_aggregator_skips_equity_tick,
           test_greeks_aggregator_uses_exchange_timestamp, test_greeks_aggregator_throttles_per_security

- [ ] 4. Create `option_greeks_live` QuestDB table (separate from API-driven table)
  - Files: crates/storage/src/greeks_persistence.rs
  - New DDL: `option_greeks_live` with same schema as `option_greeks` but
    DEDUP KEY includes timestamp granularity for per-second snapshots
  - `ts` = exchange_timestamp from tick (IST epoch secs → nanos)
  - Keep existing `option_greeks` table for Dhan API data (audit trail)
  - Tests: test_option_greeks_live_ddl_creates_table

- [ ] 5. Add PCR computation from tick OI aggregation
  - Files: crates/core/src/pipeline/greeks_aggregator.rs
  - Track running total_put_oi / total_call_oi per underlying per expiry
  - On each F&O tick update: recalculate PCR, write to `pcr_snapshots_live`
  - Timestamp = exchange_timestamp from the triggering tick
  - Tests: test_pcr_updates_on_oi_change, test_pcr_uses_exchange_timestamp

- [ ] 6. Wire GreeksAggregator into trading pipeline as broadcast consumer
  - Files: crates/app/src/trading_pipeline.rs
  - Subscribe new broadcast::Receiver<ParsedTick>
  - Spawn as background tokio task (same pattern as candle aggregator)
  - Pass instrument_registry + greeks_config + questdb_writer
  - Tests: test_greeks_aggregator_receives_broadcast_ticks

- [ ] 7. Add `source_tick_ts` column to existing API-driven Greeks tables
  - Files: crates/storage/src/greeks_persistence.rs, crates/app/src/greeks_pipeline.rs
  - For Dhan API rows: `source_tick_ts = NULL` (no exchange timestamp available)
  - For tick-driven rows: `source_tick_ts = exchange_timestamp` from tick
  - Allows clear distinction between API-sourced and tick-sourced Greeks
  - Tests: test_source_tick_ts_null_for_api_rows, test_source_tick_ts_set_for_tick_rows

## Architecture

```
WebSocket (Full mode, 162 bytes)
    │
    ▼
Tick Processor (parse binary → ParsedTick)
    │
    ├─► Tick Persistence (QuestDB: ticks table)
    │
    ├─► Broadcast Channel (ParsedTick)
    │       │
    │       ├─► Candle Aggregator (1s OHLCV → candles table)
    │       │
    │       ├─► Strategy Evaluator (indicators, signals)
    │       │
    │       └─► GreeksAggregator (NEW)
    │               │
    │               ├─ IDX_I/EQ tick → update underlying LTP cache
    │               │
    │               └─ NSE_FNO tick → lookup registry + underlying LTP
    │                       → compute_greeks(spot, strike, T, r, q, option_ltp)
    │                       → write to option_greeks_live (ts = exchange_timestamp)
    │                       → update PCR running totals
    │                       → write to pcr_snapshots_live (ts = exchange_timestamp)
    │
    └─► [Existing] Greeks Pipeline (REST API, every 60s, audit only)
```

## Timestamp Flow (the whole point)

```
NSE Exchange generates tick at 09:15:03 IST
    → Dhan WS delivers binary packet
        → tick_processor parses: exchange_timestamp = 09:15:03
            → broadcast to GreeksAggregator
                → Greeks computed, stored with ts = 09:15:03
                    → QuestDB: option_greeks_live.ts = 09:15:03
                                ticks.ts = 09:15:03
                                candles_1s.ts = 09:15:03
                    → PERFECT ALIGNMENT ✓
```

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | F&O tick arrives, underlying LTP cached | Compute Greeks, store with exchange_timestamp |
| 2 | F&O tick arrives, no underlying LTP yet | Skip (no spot price to compute with) |
| 3 | IDX_I tick arrives | Update underlying LTP cache only, no Greeks |
| 4 | NSE_EQ tick arrives | Update underlying LTP cache only |
| 5 | Same security_id, 2 ticks within 1 second | Only first triggers Greeks write (throttle) |
| 6 | Expiry day, T approaching 0 | MIN_TIME_TO_EXPIRY guard handles it (already in BS code) |
| 7 | OI changes on F&O tick | PCR running totals updated, new PCR row written |
| 8 | Query: JOIN ticks + greeks + candles on ts | All align because all use exchange_timestamp |
