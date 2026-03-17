# Implementation Plan: Resilient Historical Fetch + Post-Market WebSocket Lifecycle

**Status:** DRAFT
**Date:** 2026-03-17
**Approved by:** pending

## Plan Items

- [ ] P1: Retry-until-success for transient errors in historical fetch
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Tests: test_retry_queue_retries_failed_instruments, test_retry_queue_skips_never_retry, test_retry_queue_max_waves
  - Change: After single-pass fetch, collect all failed instruments (transient errors only — NOT NeverRetry/TokenExpired). Re-attempt failed instruments in waves with increasing backoff between waves. Max 5 waves (configurable). Only give up after all retry waves exhausted. Add `RETRY_WAVE_MAX` and `RETRY_WAVE_BACKOFF_SECS` constants. Track `ErrorAction` per-instrument so NeverRetry instruments are permanently excluded from retry queue.

- [ ] P2: Fix `.expect()` in `fetch_historical_candles` (line 215)
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Tests: (existing tests cover this path)
  - Change: Replace `.expect()` with `match` + early return, same pattern as `duration_until_post_market_fetch`

- [ ] P3: Post-market WebSocket disconnect at 15:30 IST
  - Files: crates/app/src/main.rs
  - Tests: test_post_market_monitor_constants
  - Change: Spawn a background task that sleeps until 15:30 IST (`data_collection_end`), then aborts all WebSocket JoinHandles, closes StorageGate, and sends a Telegram notification. Uses `tokio_util::sync::CancellationToken` shared between the monitor task and WebSocket connection tasks. The scheduler's `determine_phase()` + `phase_needs_websocket()` already exist — this wires them into the runtime.

- [ ] P4: Post-market disconnect triggers historical re-fetch
  - Files: crates/app/src/main.rs
  - Tests: (integration — post-market fetch already tested)
  - Change: After the post-market monitor disconnects WebSockets, it signals the historical fetch task (via a `tokio::sync::Notify` or similar) to start the re-fetch instead of the current independent sleep-until-15:35 approach. This ensures the re-fetch happens AFTER WebSocket disconnect, not on a fixed timer.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Instrument fails due to rate limit (429/DH-904) | Retried in next wave after backoff, eventually succeeds |
| 2 | Instrument fails due to 805 (too many connections) | 60s pause then retried, across multiple waves if needed |
| 3 | Instrument fails due to DH-905 (input error) | Never retried — permanently excluded from retry queue |
| 4 | Instrument fails due to 807 (token expired) | Never retried — token refresh happens elsewhere |
| 5 | All instruments succeed on first pass | No retry waves needed, zero instruments_failed |
| 6 | Some instruments fail after all 5 retry waves | instruments_failed reflects final count, Telegram sent |
| 7 | Market closes at 15:30 IST | WebSockets disconnected, StorageGate closed, Telegram sent |
| 8 | Post-market disconnect triggers re-fetch | Historical fetch starts after WS disconnect, not on fixed timer |
| 9 | System boots after 15:30 IST | No WebSocket connections started (existing behavior) |
