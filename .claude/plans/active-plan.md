# Implementation Plan: Historical Fetch — Missing Features

**Status:** VERIFIED
**Date:** 2026-03-16
**Approved by:** Parthiban ("implement everything bro")

## Plan Items

- [x] P1: Include DisplayIndex (subscribed indices like INDIA VIX) in historical fetch
  - Files: crates/core/src/historical/candle_fetcher.rs (lines 203-222)
  - Tests: test_display_index_maps_to_index_type
  - Change: Remove DisplayIndex from skip filter, add DisplayIndex => "INDEX" in instrument_type match

- [x] P2: Add HistoricalFetchComplete notification event + always send summary
  - Files: crates/core/src/notification/events.rs, crates/app/src/main.rs
  - Tests: test_historical_fetch_complete_message, test_historical_fetch_complete_is_info, test_candle_verification_passed_message, test_candle_verification_passed_is_low
  - Change: New HistoricalFetchComplete + CandleVerificationPassed variants, send on ALL completions

- [x] P3: Enhanced OHLC validation at persist time (high >= low, prices > 0)
  - Files: crates/core/src/historical/candle_fetcher.rs (persist_intraday_candles, persist_daily_candles)
  - Tests: test_ohlc_validation_rejects_high_below_low, test_ohlc_validation_rejects_non_positive
  - Change: Add non-positive price check and high < low check alongside existing NaN/Inf check

- [x] P4: Rate limit error handling — parse error responses, handle 805 and HTTP 429
  - Files: crates/core/src/historical/candle_fetcher.rs (classify_error, ErrorAction enum)
  - Tests: test_dhan_error_response_parsing_805, test_dhan_error_response_parsing_807, test_dhan_error_response_parsing_dh904, test_dhan_error_response_parsing_dh905, test_http_429_always_rate_limited, test_data_api_error_code_extraction, test_unknown_error_body_standard_retry
  - Change: Parse Dhan error JSON, 805 → 60s pause, HTTP 429 → exponential backoff, DH-905 → never retry

- [x] P5: Enhanced cross-verification — multi-timeframe, per-timeframe counts
  - Files: crates/core/src/historical/cross_verify.rs (full rewrite)
  - Tests: test_verified_timeframes_has_all_five, test_timeframe_coverage_structure, test_multi_timeframe_report_fields, test_failed_report_all_zeros
  - Change: Query all 5 timeframes, per-timeframe coverage summary, OHLC consistency check

- [x] P6: Post-market re-fetch scheduling — trigger at 15:35 IST
  - Files: crates/app/src/main.rs, crates/core/src/historical/candle_fetcher.rs (duration_until_post_market_fetch), crates/common/src/constants.rs
  - Tests: test_post_market_time_calculation, test_post_market_constants
  - Change: Add scheduler that waits until 15:35 IST, then re-runs fetch + verification with Telegram notifications

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | INDIA VIX (DisplayIndex) at boot | Fetched as INDEX across all 5 timeframes |
| 2 | Fetch completes with 0 failures | HistoricalFetchComplete Telegram sent (Low severity) |
| 3 | Fetch has partial failures | HistoricalFetchFailed Telegram sent (High severity) |
| 4 | Dhan returns high < low candle | Candle rejected with warning log |
| 5 | Dhan returns HTTP 429 | Exponential backoff 10s→20s→40s→80s |
| 6 | Dhan returns error code 805 | ALL requests pause 60s, then resume |
| 7 | Cross-verify finds 1m gaps but 5m OK | Report shows per-timeframe breakdown |
| 8 | System running at 15:35 IST | Post-market fetch triggers automatically |
| 9 | System boots after 15:35 IST | Post-market fetch runs immediately |
