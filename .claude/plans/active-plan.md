# Implementation Plan: Historical Candle Fetch Hardening

**Status:** DRAFT
**Date:** 2026-03-18
**Approved by:** pending

## Problem Statement

Deep analysis revealed 6 critical gaps in the historical candle system:
1. Cross-verify only checks `high < low` — no open/close/volume/timestamp validation
2. `instruments_checked == 0` → verification passes (empty DB = OK?!)
3. QuestDB append failures silently lost — not counted, not alerted
4. Telegram only shows failed count, not WHICH instruments failed
5. No timestamp bounds validation (candles outside market hours undetected)
6. Persist errors count candle as success (increments count even on write failure)

## Plan Items

- [ ] 1. Enhance cross-verify with full OHLCV + timestamp validation
  - Files: crates/core/src/historical/cross_verify.rs
  - Add SQL query: `WHERE (open <= 0 OR close <= 0 OR volume < 0)` → count violations
  - Add SQL query: timestamp bounds check for intraday (IST 03:45:00 - 10:00:00 UTC = 09:15-15:30 IST)
  - Add `data_violations` field to `CrossVerificationReport`
  - Tests: test_cross_verify_report_includes_data_violations, test_zero_instruments_fails_verification

- [ ] 2. Fix zero-instruments-passes-verification bug
  - Files: crates/core/src/historical/cross_verify.rs
  - Change: `instruments_checked == 0` → `passed = false` (not true)
  - Tests: test_zero_instruments_fails_verification

- [ ] 3. Track failed instrument names in CandleFetchSummary
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Add `failed_instruments: Vec<String>` field to `CandleFetchSummary`
  - Collect security_id + segment for each instrument that fails all retry waves
  - Cap at 50 names to prevent unbounded allocation
  - Tests: test_candle_fetch_summary_tracks_failed_names

- [ ] 4. Fix QuestDB persist failure counting
  - Files: crates/core/src/historical/candle_fetcher.rs
  - In `persist_intraday_candles` and `persist_daily_candles`: do NOT increment count on append error
  - Add `persist_failures` counter to track silently-lost candles
  - Tests: test_persist_failure_not_counted_as_success

- [ ] 5. Enhance Telegram notifications with failed instrument details
  - Files: crates/core/src/notification/events.rs, crates/app/src/main.rs
  - Add `failed_instruments: Vec<String>` to `HistoricalFetchFailed` event
  - Add `data_violations` to `CandleVerificationFailed` event
  - Format: show up to 10 failed instrument names in Telegram message
  - Tests: test_historical_fetch_failed_shows_instrument_names, test_candle_verification_failed_shows_data_violations

- [ ] 6. Add comprehensive tests for all changes
  - Files: crates/core/src/historical/cross_verify.rs, crates/core/src/historical/candle_fetcher.rs, crates/core/src/notification/events.rs
  - Tests: all listed above + test_data_violation_query_structure, test_timestamp_bounds_query

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 3 instruments fail after all retry waves | Telegram shows "Failed: 3" + names: "RELIANCE (NSE_EQ), TCS (NSE_EQ), NIFTY50 (IDX_I)" |
| 2 | QuestDB down during persist | Persist failures counted separately, not inflating total_candles |
| 3 | Empty DB (zero instruments) | Verification FAILS, Telegram sends High alert |
| 4 | Candles with zero price in DB | Data violations count > 0, shown in verification report |
| 5 | All instruments succeed | Normal flow, "Historical candles OK" notification |
