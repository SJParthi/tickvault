# Implementation Plan: Historical Pipeline Resilience — Close All Gaps

**Status:** VERIFIED
**Date:** 2026-03-18
**Approved by:** Parthiban

## Problem Statement

Deep analysis identified 11 gaps in the historical candle pipeline. Token expiry
mid-fetch silently abandons instruments, QuestDB failures are counted but not
circuit-broken, cross-match uses fixed absolute tolerance, volume/OI aren't
cross-matched, and violation details are capped globally instead of per-timeframe.

## Plan Items

- [x] 1. C1: Token expiry recovery — new enum variant + retry wave awareness
  - Files: candle_fetcher.rs
  - Changes:
    - Add `InstrumentFetchResult::TokenExpired` variant
    - When `ErrorAction::TokenExpired` detected, return `TokenExpired` (not `Failed`)
    - In retry wave loop: if any result is `TokenExpired`, sleep 30s for token refresh then retry
    - Token handle re-read on every attempt (already happens via `token_handle.load()`)
    - If token still expired after 2 retries → mark as `Failed` (give up)
    - Add metric `tv_historical_token_expired_total`
  - Tests: test_token_expired_variant_exists, test_token_expired_distinct_from_failed

- [x] 2. C2: Consecutive persist failure circuit breaker
  - Files: candle_fetcher.rs
  - Changes:
    - Add constant `MAX_CONSECUTIVE_PERSIST_FAILURES = 10`
    - In `persist_intraday_candles` and `persist_daily_candles`: track consecutive failures
    - If consecutive persist failures >= 10, break out of loop early and log error
    - Return the partial count + failures so caller knows
  - Tests: test_consecutive_persist_failure_threshold, test_persist_circuit_breaker_resets_on_success

- [x] 3. C3: Zero tolerance — any price diff is a real mismatch
  - Files: cross_verify.rs
  - Changes:
    - f32→f64 already fixed via `f32_to_f64_clean()`. Any diff = real problem.
    - Replace `CROSS_MATCH_PRICE_TOLERANCE = 0.01` with epsilon (1e-6) for IEEE754 noise only
    - SQL uses `abs(h.open - m.open) > 0.000001` — catches everything meaningful
    - Min tick in Indian markets = 0.05, so any real diff >> 1e-6
  - Tests: test_cross_match_epsilon_tolerance, test_epsilon_catches_real_tick_diff

- [x] 4. H1: Add volume to cross-match mismatch detection
  - Files: cross_verify.rs
  - Changes:
    - Add `CROSS_MATCH_VOLUME_TOLERANCE_PCT = 0.10` (10% relative)
    - In Rust-side filtering: check `abs(h_vol - m_vol) / max(h_vol, 1) > 0.10`
    - Volume mismatch alone triggers mismatch (not just OHLC)
  - Tests: test_cross_match_volume_tolerance_constant

- [x] 5. H2: Add OI to cross-match for derivatives
  - Files: cross_verify.rs
  - Changes:
    - SQL: include `h.open_interest, m.open_interest` in SELECT
    - Rust-side: if both OI > 0 and differ by > 10%, flag as mismatch
    - Add `oi_mismatches: usize` to `CrossMatchReport`
  - Tests: test_cross_match_report_includes_oi_mismatches_field

- [x] 6. H4: QuestDB unreachable during verification → error-level log
  - Files: cross_verify.rs
  - Changes:
    - `execute_query` on connection failure: change `warn!` to `tracing::error!`
    - This triggers Telegram alert via the ERROR-level tracing subscriber
  - Tests: (existing test_verify_candle_integrity_unreachable_host covers path)

- [x] 7. M1: Show ALL violations — no arbitrary cap
  - Files: cross_verify.rs
  - Changes:
    - Remove `LIMIT 20` from violation detail queries
    - Add `MAX_VIOLATION_DETAILS = 500` as memory safety cap only (not for hiding)
    - Total count always accurate from separate count query
    - ALL violations returned in report struct
    - Telegram shows all of them — if 500 violations, you see 500
  - Tests: test_violation_detail_no_arbitrary_cap

- [x] 8. M2: Missing materialized view detection
  - Files: cross_verify.rs
  - Changes:
    - Before cross-match JOIN, check if each live view table exists via:
      `SELECT count() FROM tables() WHERE name = '{table_name}'`
    - Add `missing_views: Vec<String>` to `CrossMatchReport`
    - If view missing, skip that timeframe but record it
  - Tests: test_cross_match_report_includes_missing_views_field

- [x] 9. M4: Per-instrument error reason tracking
  - Files: candle_fetcher.rs
  - Changes:
    - Add `failure_reasons: HashMap<String, usize>` to `CandleFetchSummary`
    - Track categories: "token_expired", "rate_limited", "input_error", "network", "persist"
    - Wire from `InstrumentFetchResult` variants + persist failures
  - Tests: test_candle_fetch_summary_tracks_failure_reasons

- [x] 10. Doc fix: Update mod.rs dedup key comment
  - Files: mod.rs
  - Tests: (doc comment only)

- [x] 11. Run fmt + clippy + test
  - Files: all modified
  - Tests: full workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Token expires at instrument 50/232 | Returns TokenExpired, wave sleeps 30s, token refreshed, remaining instruments recovered |
| 2 | QuestDB down during persist | After 10 consecutive failures, breaks loop, instrument marked Failed |
| 3 | Penny stock Rs.1.00, diff 0.05 | diff 0.05 > 1e-6 epsilon → FLAGGED (real tick diff) |
| 4 | NIFTY Rs.23500, diff 0.000001 | diff = 1e-6 = epsilon → passes (IEEE754 noise) |
| 5 | Volume 50K live vs 100K hist | 50% diff > 10% tolerance → flagged |
| 6 | Missing candles_5m view | Reported in missing_views, that timeframe skipped |
| 7 | 50 violations across timeframes | ALL 50 shown — nothing hidden |

## Principles Check

| Principle | Compliance |
|-----------|-----------|
| Zero allocation on hot path | YES — all cold path (boot + post-market) |
| O(1) or fail at compile time | YES — SQL capped with LIMIT, loops bounded |
| Every version pinned | YES — no new dependencies |
