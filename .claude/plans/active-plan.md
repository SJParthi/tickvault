# Implementation Plan: Greeks Data Pipeline (Write Pipeline)

**Status:** VERIFIED
**Date:** 2026-03-23
**Approved by:** Parthiban

## Problem

All 4 greeks tables (option_greeks, dhan_option_chain_raw, greeks_verification, pcr_snapshots) are created on boot via DDL but have 0 rows — no data write pipeline exists.

## Existing Infrastructure (Ready to Use)

- `OptionChainClient` (core/option_chain/client.rs) — fetches expiry list + chain from Dhan API, rate-limited at 3s
- `compute_greeks()` (trading/greeks/black_scholes.rs) — Black-Scholes engine, O(1)
- `compute_pcr()` / `classify_pcr()` (trading/greeks/pcr.rs) — PCR computation
- `classify_buildup()` (trading/greeks/buildup.rs) — OI buildup classification
- `GreeksConfig` (common/config.rs) — risk_free_rate, dividend_yield, iv_solver params
- `VALIDATION_MUST_EXIST_INDICES` (common/constants.rs) — NIFTY=13, BANKNIFTY=25, FINNIFTY=27, MIDCPNIFTY=442, etc.

## Plan Items

- [x] 1. Add `enabled` and `fetch_interval_secs` fields to `GreeksConfig`
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_greeks_config_defaults (existing Default impl test covers new fields)

- [x] 2. Add ILP write functions to `greeks_persistence.rs`
  - `GreeksPersistenceWriter` struct (ILP sender + buffer, like TickPersistenceWriter but simpler — cold path, no ring buffer)
  - `build_dhan_raw_row()` — writes raw Dhan API response to `dhan_option_chain_raw`
  - `build_option_greeks_row()` — writes computed Greeks to `option_greeks`
  - `build_pcr_snapshot_row()` — writes PCR snapshot to `pcr_snapshots`
  - `build_verification_row()` — writes cross-verification to `greeks_verification`
  - `flush()` — sends buffer to QuestDB
  - Files: crates/storage/src/greeks_persistence.rs
  - Tests: 20+ DDL/schema validation tests in greeks_persistence::tests

- [x] 3. Create `greeks_pipeline.rs` orchestrator in app crate
  - `run_greeks_pipeline()` async function that loops every `fetch_interval_secs`:
    a. For each underlying in VALIDATION_MUST_EXIST_INDICES (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY):
       - Fetch expiry list → pick nearest expiry
       - Fetch option chain for that expiry
       - For each strike in chain:
         - Write raw data to `dhan_option_chain_raw`
         - Compute our Greeks (Black-Scholes with spot, strike, time_to_expiry, risk_free_rate, dividend_yield)
         - Write computed Greeks to `option_greeks`
         - Compare our vs Dhan Greeks → write to `greeks_verification`
       - Aggregate CE/PE OI and volume → compute PCR → write to `pcr_snapshots`
    b. Flush all writes
    c. Sleep until next interval
  - Files: crates/app/src/greeks_pipeline.rs
  - Tests: test_compute_time_to_expiry_*, test_classify_match_*

- [x] 4. Wire greeks pipeline in main.rs boot sequence
  - After auth is ready and QuestDB tables created
  - Spawn `tokio::spawn(run_greeks_pipeline(...))`
  - Pass: TokenHandle, client_id, dhan_base_url, GreeksConfig, QuestDbConfig
  - Files: crates/app/src/main.rs (both fast boot and regular boot paths)
  - Tests: (integration — verified by compilation + running system)

- [x] 5. Add module declaration in app crate
  - Files: crates/app/src/lib.rs
  - Tests: compilation passes

## Data Flow

```
OptionChainClient.fetch_expiry_list()
        |
        v
OptionChainClient.fetch_option_chain()
        |
        v  (for each strike)
   +----+----+----+
   |    |    |    |
   v    v    v    v
 Raw  Greeks Verif  PCR
 dhan_ option_ greeks_ pcr_
 chain greeks  verif   snap
```

## Rate Limit Budget

- 4 underlyings × (1 expiry_list + 1 chain) = 8 API calls per cycle
- At 3s rate limit = ~24 seconds per cycle
- Default fetch_interval_secs = 60 (safe margin)
- Dhan Data API: 5/sec limit (we do 1/3sec — well within)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Normal market hours | All 4 tables populated every 60s |
| 2 | QuestDB down | Writer reconnects, logs warn, no crash |
| 3 | Dhan API error | Log error, skip this cycle, retry next |
| 4 | No token available | Log error, skip, retry next cycle |
| 5 | greeks.enabled = false | Pipeline not spawned |
| 6 | Outside market hours | Pipeline still runs (useful for testing/dev) |
