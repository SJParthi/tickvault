# Implementation Plan: Historical Pipeline Resilience — Close All Gaps

**Status:** DRAFT
**Date:** 2026-03-18
**Approved by:** pending

## Problem Statement

Deep analysis identified 11 gaps in the historical candle pipeline. Token expiry
mid-fetch silently abandons instruments, QuestDB failures are counted but not
circuit-broken, cross-match uses fixed absolute tolerance, volume/OI aren't
cross-matched, and violation details are capped globally instead of per-timeframe.

## Plan Items

- [ ] 1. C1: Token expiry recovery — new enum variant + retry wave awareness
  - Files: candle_fetcher.rs
  - Changes:
    - Add `InstrumentFetchResult::TokenExpired` variant
    - When `ErrorAction::TokenExpired` detected, return `TokenExpired` (not `Failed`)
    - In retry wave loop: if any result is `TokenExpired`, sleep 30s for token refresh then retry
    - Token handle re-read on every attempt (already happens via `token_handle.load()`)
    - If token still expired after 2 retries → mark as `Failed` (give up)
    - Add metric `dlt_historical_token_expired_total`
  - Tests: test_token_expired_variant_exists, test_token_expired_distinct_from_failed

- [ ] 2. C2: Consecutive persist failure circuit breaker
  - Files: candle_fetcher.rs
  - Changes:
    - Add constant `MAX_CONSECUTIVE_PERSIST_FAILURES = 10`
    - In `persist_intraday_candles` and `persist_daily_candles`: track consecutive failures
    - If consecutive persist failures >= 10, break out of loop early and log error
    - Return the partial count + failures so caller knows
  - Tests: test_consecutive_persist_failure_threshold, test_persist_circuit_breaker_resets_on_success

- [ ] 3. C3: Relative price tolerance for cross-match
  - Files: cross_verify.rs
  - Changes:
    - Replace fixed `CROSS_MATCH_PRICE_TOLERANCE = 0.01` with a function:
      `fn price_tolerance(price: f64) -> f64 { f64::max(0.01, price.abs() * 0.0005) }`
    - Since SQL can't call Rust functions, use two-pass approach:
      - SQL fetches all joined rows with price data
      - Rust-side applies relative tolerance filtering
    - OR: simpler approach — keep SQL with a generous absolute tolerance (e.g., 0.5)
      as pre-filter, then apply relative tolerance in Rust on the fetched rows
  - Tests: test_relative_price_tolerance_penny_stock, test_relative_price_tolerance_large_cap,
           test_price_tolerance_function_min_floor

- [ ] 4. H1: Add volume to cross-match mismatch detection
  - Files: cross_verify.rs
  - Changes:
    - Add `CROSS_MATCH_VOLUME_TOLERANCE_PCT = 0.10` (10% relative)
    - In Rust-side filtering: check `abs(h_vol - m_vol) / max(h_vol, 1) > 0.10`
    - Volume mismatch alone triggers mismatch (not just OHLC)
  - Tests: test_cross_match_volume_tolerance_constant

- [ ] 5. H2: Add OI to cross-match for derivatives
  - Files: cross_verify.rs
  - Changes:
    - SQL: include `h.open_interest, m.open_interest` in SELECT
    - Rust-side: if both OI > 0 and differ by > 10%, flag as mismatch
    - Add `oi_mismatches: usize` to `CrossMatchReport`
  - Tests: test_cross_match_report_includes_oi_mismatches_field

- [ ] 6. H4: QuestDB unreachable during verification → error-level log
  - Files: cross_verify.rs
  - Changes:
    - `execute_query` on connection failure: change `warn!` to `tracing::error!`
    - This triggers Telegram alert via the ERROR-level tracing subscriber
  - Tests: (existing test_verify_candle_integrity_unreachable_host covers path)

- [ ] 7. M1: Per-timeframe violation detail cap
  - Files: cross_verify.rs
  - Changes:
    - Change MAX_VIOLATION_DETAILS from 20 global to 4 per timeframe × 5 = 20 total
    - In violation detail queries: `LIMIT 4` per timeframe, aggregate up to 20
    - Ensures all 5 timeframes get representation
  - Tests: test_per_timeframe_violation_detail_limit

- [ ] 8. M2: Missing materialized view detection
  - Files: cross_verify.rs
  - Changes:
    - Before cross-match JOIN, check if each live view table exists via:
      `SELECT count() FROM tables() WHERE name = '{table_name}'`
    - Add `missing_views: Vec<String>` to `CrossMatchReport`
    - If view missing, skip that timeframe but record it
  - Tests: test_cross_match_report_includes_missing_views_field

- [ ] 9. M4: Per-instrument error reason tracking
  - Files: candle_fetcher.rs
  - Changes:
    - Add `failure_reasons: HashMap<String, usize>` to `CandleFetchSummary`
    - Track categories: "token_expired", "rate_limited", "input_error", "network", "persist"
    - Wire from `InstrumentFetchResult` variants + persist failures
  - Tests: test_candle_fetch_summary_tracks_failure_reasons

- [ ] 10. Doc fix: Update mod.rs dedup key comment
  - Files: mod.rs
  - Tests: (doc comment only)

- [ ] 11. Run fmt + clippy + test
  - Files: all modified
  - Tests: full workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Token expires at instrument 50/232 | Returns TokenExpired, wave sleeps 30s, token refreshed, remaining instruments recovered |
| 2 | QuestDB down during persist | After 10 consecutive failures, breaks loop, instrument marked Failed |
| 3 | Penny stock Rs.1.00, diff 0.005 | tolerance = max(0.01, 1.0 * 0.0005) = 0.01 → passes |
| 4 | NIFTY Rs.23500, diff 0.02 | tolerance = max(0.01, 23500 * 0.0005) = 11.75 → passes (f32 rounding) |
| 5 | Volume 50K live vs 100K hist | 50% diff > 10% tolerance → flagged |
| 6 | Missing candles_5m view | Reported in missing_views, that timeframe skipped |
| 7 | All 20 violations from 1m only | Now: 4 from each timeframe, all 5 represented |

## Principles Check

| Principle | Compliance |
|-----------|-----------|
| Zero allocation on hot path | YES — all cold path (boot + post-market) |
| O(1) or fail at compile time | YES — SQL capped with LIMIT, loops bounded |
| Every version pinned | YES — no new dependencies |
