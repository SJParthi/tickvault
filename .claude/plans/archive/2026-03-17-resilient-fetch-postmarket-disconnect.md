# Implementation Plan: Resilient Historical Fetch + Post-Market WebSocket Lifecycle

**Status:** VERIFIED
**Date:** 2026-03-17
**Approved by:** Parthiban ("go ahead with all the plans dude")

## Plan Items

- [x] P1: Retry-until-success for transient errors in historical fetch
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Tests: test_retry_wave_constants, test_retry_wave_backoff_sequence, test_instrument_fetch_result_variants, test_retry_wave_max_total_backoff
  - Change: Restructured fetch loop with retry waves. After initial pass, re-attempts ALL failed instruments in up to 5 waves with increasing backoff (30s, 60s, 90s, 120s, 150s). Extracted per-instrument fetch into `fetch_single_instrument()` returning `InstrumentFetchResult`. NeverRetry errors (DH-905) fail fast on re-attempt (no wasted API calls).

- [x] P2: Fix `.expect()` in `fetch_historical_candles` (line 215)
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Tests: (existing tests cover this path)
  - Change: Replaced `.expect()` with `match` + early return returning empty summary

- [x] P3: Post-market WebSocket disconnect at 15:30 IST
  - Files: crates/app/src/main.rs
  - Tests: test_compute_market_close_sleep_valid_time, test_compute_market_close_sleep_invalid_format, test_compute_market_close_sleep_empty_string, test_post_market_monitor_constants
  - Change: `run_shutdown_fast` now uses `tokio::select!` between Ctrl+C and market close timer. At market close: aborts WS handles + order update + trading pipeline, sends Telegram notification, but keeps API/dashboard running. Second Ctrl+C triggers full shutdown. Added `compute_market_close_sleep()` helper.

- [x] P4: Post-market disconnect triggers historical re-fetch
  - Files: crates/app/src/main.rs
  - Tests: (integration — wired via tokio::sync::Notify)
  - Change: Added `tokio::sync::Notify` shared between `run_shutdown_fast` and the historical fetch spawner. Post-market disconnect fires `notify_one()`, historical fetch task waits on `select!` between the notify and 15:35 IST timer. After disconnect signal, adds 5-minute buffer for Dhan data finalization before re-fetch.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Instrument fails due to rate limit (429/DH-904) | Retried in next wave after backoff, eventually succeeds |
| 2 | Instrument fails due to 805 (too many connections) | 60s pause then retried, across multiple waves if needed |
| 3 | Instrument fails due to DH-905 (input error) | Fails fast on retry (NeverRetry returns immediately) |
| 4 | Instrument fails due to 807 (token expired) | Fails fast on retry (TokenExpired returns immediately) |
| 5 | All instruments succeed on first pass | No retry waves needed, zero instruments_failed |
| 6 | Some instruments fail after all 5 retry waves | instruments_failed reflects final count, Telegram sent |
| 7 | Market closes at 15:30 IST | WebSockets disconnected, Telegram sent, API stays up |
| 8 | Post-market disconnect triggers re-fetch | Historical fetch starts 5min after WS disconnect signal |
| 9 | System boots after 15:30 IST | No WebSocket connections started (existing behavior) |
