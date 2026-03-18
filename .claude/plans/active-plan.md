# Implementation Plan: Historical Candle Fetch Hardening — Complete Coverage

**Status:** DRAFT
**Date:** 2026-03-18
**Approved by:** pending

## Problem Statement

Deep analysis revealed critical gaps in the historical candle system. These must be
fixed so that **no data is ever silently lost** and **Telegram notifications alone are
enough to diagnose any problem**.

### Gaps Found

| # | Gap | Severity | Current Behavior | Required Behavior |
|---|-----|----------|-----------------|-------------------|
| 1 | Cross-verify only checks `high < low` | CRITICAL | Bad OHLCV data persists undetected | Full OHLCV + timestamp validation |
| 2 | `instruments_checked == 0` passes | CRITICAL | Empty DB = verification OK | Must FAIL |
| 3 | QuestDB append failures silent | HIGH | Candle lost, count incremented anyway | Track persist failures, alert via Telegram |
| 4 | Telegram shows only failed COUNT | HIGH | "Failed: 3" — which instruments? | Show instrument names + segments |
| 5 | No timestamp bounds check | HIGH | Candles outside market hours undetected | Validate 09:15-15:29 IST for intraday |
| 6 | No per-error-type Telegram detail | MEDIUM | Just "partial failure" | Show error breakdown: rate limit / token / persist |

## Plan Items

- [ ] 1. Enhance `CrossVerificationReport` with comprehensive validation
  - Files: crates/core/src/historical/cross_verify.rs
  - Changes:
    - Add new field `data_violations: usize` (count of candles with open/close <= 0 or volume < 0)
    - Add new field `timestamp_violations: usize` (intraday candles outside market hours)
    - Add SQL query: `WHERE timeframe != '1d' AND (open <= 0 OR close <= 0 OR high <= 0 OR low <= 0)` → count
    - Add SQL query: `WHERE timeframe != '1d' AND volume < 0` → count
    - Include all violation counts in pass/fail logic: `passed = gaps==0 && ohlc==0 && data==0 && timestamp==0 && checked>0`
  - Tests: test_cross_verify_report_includes_data_violations, test_cross_verify_report_includes_timestamp_violations

- [ ] 2. Fix zero-instruments-passes-verification bug
  - Files: crates/core/src/historical/cross_verify.rs
  - Change line 241: remove `|| instruments_checked == 0` from pass condition
  - New logic: `instruments_checked == 0` → `passed = false` (verification FAILS on empty DB)
  - Tests: test_zero_instruments_fails_verification

- [ ] 3. Track failed instrument names in `CandleFetchSummary`
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Add `failed_instruments: Vec<String>` field to `CandleFetchSummary`
  - Format: `"{security_id} ({segment})"` e.g., `"11536 (NSE_EQ)"`
  - After all retry waves, collect names from `pending_indices` (the instruments that never succeeded)
  - Cap at `MAX_FAILED_NAMES = 50` to prevent unbounded allocation
  - Tests: test_candle_fetch_summary_includes_failed_names

- [ ] 4. Fix QuestDB persist failure counting — don't count failed writes as success
  - Files: crates/core/src/historical/candle_fetcher.rs
  - In `persist_intraday_candles`: move `count += 1` inside success branch of `append_candle`
  - In `persist_daily_candles`: same fix
  - Add `persist_failures: usize` return alongside count
  - Return `(count, persist_failures)` tuple from both persist functions
  - Add `persist_failures` field to `CandleFetchSummary`
  - Tests: test_persist_failure_not_counted_as_success

- [ ] 5. Enhance Telegram notifications — make them self-contained diagnostic reports
  - Files: crates/core/src/notification/events.rs, crates/app/src/main.rs
  - A. `HistoricalFetchFailed` — add fields:
    - `failed_instruments: Vec<String>` (up to 10 shown in message, with "+N more" if >10)
    - `persist_failures: usize` (QuestDB write failures)
    - Message format:
      ```
      Historical candle fetch — partial failure
      Fetched: 200
      Failed: 9
      Candles: 180000
      Persist errors: 42

      Failed instruments:
      • 11536 (NSE_EQ)
      • 13 (IDX_I)
      • 25 (IDX_I)
      ... +6 more
      ```
  - B. `HistoricalFetchComplete` — add field:
    - `persist_failures: usize`
    - If persist_failures > 0, show in message (even on "success" — candles were fetched but some lost during write)
  - C. `CandleVerificationFailed` — add fields:
    - `data_violations: usize` (non-positive OHLC values)
    - `timestamp_violations: usize` (out-of-market-hours candles)
    - Message format:
      ```
      Candle verification FAILED
      Checked: 209
      With gaps: 3
      OHLC violations: 0
      Data violations: 5
      Timestamp violations: 2

      Timeframes:
      1m: 78,000 (207 inst)
      ...
      ```
  - D. `CandleVerificationPassed` — add `data_violations` + `timestamp_violations` fields
    - If any violations > 0, still show them (informational — verification passed but with warnings)
  - Tests: test_historical_fetch_failed_shows_instrument_names, test_historical_fetch_failed_shows_persist_errors, test_candle_verification_failed_shows_all_violations, test_candle_verification_passed_shows_warnings

- [ ] 6. Wire new fields through main.rs
  - Files: crates/app/src/main.rs
  - Pass `summary.failed_instruments` to `HistoricalFetchFailed` event
  - Pass `summary.persist_failures` to both `HistoricalFetchComplete` and `HistoricalFetchFailed`
  - Pass `verify_report.data_violations` and `verify_report.timestamp_violations` to verification events
  - Same wiring for post-market re-fetch path
  - Tests: (covered by notification event tests)

- [ ] 7. Add comprehensive unit tests for all changes
  - Files: crates/core/src/historical/cross_verify.rs, crates/core/src/historical/candle_fetcher.rs, crates/core/src/notification/events.rs
  - Cross-verify tests:
    - test_cross_verify_report_includes_data_violations
    - test_cross_verify_report_includes_timestamp_violations
    - test_zero_instruments_fails_verification
    - test_passed_requires_no_violations_of_any_type
  - Candle fetcher tests:
    - test_candle_fetch_summary_includes_failed_names
    - test_persist_failure_not_counted_as_success
    - test_failed_names_capped_at_max
  - Notification tests:
    - test_historical_fetch_failed_shows_instrument_names
    - test_historical_fetch_failed_shows_persist_errors
    - test_historical_fetch_failed_truncates_long_list
    - test_candle_verification_failed_shows_all_violations
    - test_candle_verification_passed_shows_warnings
    - test_historical_fetch_complete_shows_persist_warnings

## Scenarios

| # | Scenario | Telegram Message |
|---|----------|-----------------|
| 1 | 3 instruments fail all retries | "Failed: 3\nPersist errors: 0\n\nFailed instruments:\n• 11536 (NSE_EQ)\n• 13 (IDX_I)\n• 25 (IDX_I)" |
| 2 | QuestDB down during persist | "Fetched: 200\nFailed: 0\nCandles: 180000\nPersist errors: 42" — shows data was fetched but 42 writes failed |
| 3 | Empty DB (zero instruments) | "Candle verification FAILED\nChecked: 0\nWith gaps: 0" — verification FAILS, High alert |
| 4 | Candles with bad prices in DB | "Data violations: 5\nTimestamp violations: 0" — shown in verification report |
| 5 | All good + minor persist errors | "Historical candles OK\nFetched: 232\nCandles: 187500\nPersist errors: 2" — success but warnings visible |
| 6 | Rate limit during fetch | Instruments retry via wave system; if ultimately fail, names show in "Failed instruments" list |
| 7 | Token expires mid-fetch | Instrument skipped, appears in "Failed instruments" list with security_id |
| 8 | Post-market re-fetch succeeds | Second round of identical notifications, independently verifiable |

## Files Modified (7 total)

| File | Changes |
|------|---------|
| `crates/core/src/historical/cross_verify.rs` | +3 SQL queries, +2 fields, fix pass logic |
| `crates/core/src/historical/candle_fetcher.rs` | +failed_instruments, +persist_failures, fix count bug |
| `crates/core/src/notification/events.rs` | +fields on 4 events, update message formatting |
| `crates/app/src/main.rs` | Wire new fields to notification events (both initial + post-market) |
| (tests inline in above files) | ~15 new tests |

## Principles Check

| Principle | Compliance |
|-----------|-----------|
| Zero allocation on hot path | YES — all changes are cold path (boot + background task) |
| O(1) or fail at compile time | YES — SQL queries are O(n) but run in QuestDB (cold path) |
| Every version pinned | YES — no new dependencies |
