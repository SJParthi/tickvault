# Implementation Plan: Gate Historical Fetch by Trading Day + Market Close

**Status:** VERIFIED
**Date:** 2026-03-19
**Approved by:** Parthiban (explicit "yes correct go ahead")

## Context

Historical candle fetch currently runs immediately at boot regardless of trading day or time.
Correct behavior:
- **Non-trading day**: fetch immediately at boot
- **Trading day**: ONLY after 15:30 IST, after WS disconnect + live data fully ingested

## Plan Items

- [x] Pass `is_trading_day` to `spawn_historical_candle_fetch()`
  - Files: crates/app/src/main.rs (lines 575, 839, 1169)
  - Tests: existing boot tests pass

- [x] Restructure async task: skip initial fetch on trading days, only wait for post-market signal
  - Files: crates/app/src/main.rs (lines 1230-1394 rewritten)
  - Tests: existing pipeline tests pass

- [x] On non-trading days: run initial fetch, skip post-market wait and cross-match
  - Files: crates/app/src/main.rs
  - Tests: existing pipeline tests pass

- [x] Run full test suite and verify
  - Files: none
  - Tests: cargo test --workspace — all pass, 0 failures
