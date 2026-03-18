# Implementation Plan: Historical Candle Fetch Hardening — Precise Diagnostics

**Status:** DRAFT
**Date:** 2026-03-18
**Approved by:** pending

## Problem Statement

The historical candle system silently loses data and provides insufficient diagnostic
information. Telegram notifications must be **self-contained diagnostic reports** showing
exactly WHICH symbol, at WHICH timestamp, with WHICH OHLCV values are wrong — so you
can diagnose the issue just by reading the notification.

### Gaps Found

| # | Gap | Severity | Current | Required |
|---|-----|----------|---------|----------|
| 1 | Cross-verify only checks `high < low` count | CRITICAL | "OHLC violations: 2" — which symbols? which timestamps? | Show symbol, timestamp, and actual OHLCV values |
| 2 | `instruments_checked == 0` passes | CRITICAL | Empty DB = OK | Must FAIL |
| 3 | QuestDB append failures silent | HIGH | Candle lost, count inflated | Track persist failures, alert via Telegram |
| 4 | Telegram shows only failed COUNT | HIGH | "Failed: 3" | Show symbol names + segments |
| 5 | No timestamp bounds check | HIGH | Out-of-hours candles undetected | Validate 09:15-15:29 IST for intraday |
| 6 | No OHLCV value details | HIGH | Just a count | Show exact values per violation |

## Architecture: How Symbol Names Get Into Notifications

```
QuestDB query → returns security_id + timestamp + OHLCV values
                    ↓
InstrumentRegistry.get(security_id) → display_label (e.g., "RELIANCE", "NIFTY50")
                    ↓
CrossVerificationReport.violation_details: Vec<ViolationDetail>
                    ↓
main.rs formats for Telegram using display_label + timestamp + values
```

## New Type: `ViolationDetail`

```rust
/// A single candle that failed cross-verification, with full diagnostic context.
pub struct ViolationDetail {
    /// Human-readable symbol (e.g., "RELIANCE", "NIFTY50"). From registry lookup.
    pub symbol: String,
    /// Exchange segment string (e.g., "NSE_EQ", "IDX_I").
    pub segment: String,
    /// Timeframe (e.g., "1m", "5m", "1d").
    pub timeframe: String,
    /// Timestamp formatted as IST string (e.g., "2026-03-18 10:15").
    pub timestamp_ist: String,
    /// What's wrong (e.g., "high < low", "open <= 0", "outside market hours").
    pub violation: String,
    /// Actual OHLCV values for context (e.g., "O=0.0 H=2450.5 L=2440.0 C=2445.0").
    pub values: String,
}
```

## Plan Items

- [ ] 1. Add `ViolationDetail` struct and detailed SQL queries to cross-verify
  - Files: crates/core/src/historical/cross_verify.rs
  - Changes:
    - Add `ViolationDetail` struct (above)
    - Add `ohlc_details: Vec<ViolationDetail>` to `CrossVerificationReport` (capped at 20)
    - Add `data_details: Vec<ViolationDetail>` to `CrossVerificationReport` (capped at 20)
    - Add `timestamp_details: Vec<ViolationDetail>` to `CrossVerificationReport` (capped at 20)
    - Add `data_violations: usize` count field
    - Add `timestamp_violations: usize` count field
    - **OHLC detail query** — returns actual rows, not just count:
      ```sql
      SELECT security_id, segment, timeframe, ts, open, high, low, close, volume
      FROM historical_candles
      WHERE ts > dateadd('d', -1, now()) AND high < low
      LIMIT 20
      ```
    - **Data violation detail query** (non-positive prices):
      ```sql
      SELECT security_id, segment, timeframe, ts, open, high, low, close, volume
      FROM historical_candles
      WHERE ts > dateadd('d', -1, now())
        AND timeframe != '1d'
        AND (open <= 0 OR high <= 0 OR low <= 0 OR close <= 0)
      LIMIT 20
      ```
    - **Timestamp violation detail query** (intraday candles outside 09:15-15:29 IST):
      QuestDB stores IST-as-UTC timestamps. Market hours: 03:45 UTC to 09:59 UTC (= 09:15-15:29 IST).
      ```sql
      SELECT security_id, segment, timeframe, ts, open, high, low, close, volume
      FROM historical_candles
      WHERE ts > dateadd('d', -1, now())
        AND timeframe != '1d'
        AND (hour(ts) < 3 OR hour(ts) > 10
             OR (hour(ts) = 3 AND minute(ts) < 45)
             OR (hour(ts) = 10 AND minute(ts) > 0))
      LIMIT 20
      ```
    - **Count queries** remain for total counts (LIMIT 20 rows won't give total):
      ```sql
      SELECT count() FROM historical_candles WHERE ... AND (open <= 0 OR ...)
      SELECT count() FROM historical_candles WHERE ... AND (timestamp outside hours)
      ```
    - Pass `InstrumentRegistry` to `verify_candle_integrity()` for security_id → symbol lookup
    - Update pass/fail: `passed = gaps==0 && ohlc==0 && data==0 && timestamp==0 && checked>0`
  - Tests: test_violation_detail_struct, test_cross_verify_report_new_fields

- [ ] 2. Fix zero-instruments-passes-verification bug
  - Files: crates/core/src/historical/cross_verify.rs
  - Remove `|| instruments_checked == 0` from pass condition
  - `instruments_checked == 0` → `passed = false`
  - Tests: test_zero_instruments_fails_verification

- [ ] 3. Track failed instrument names in `CandleFetchSummary`
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Add `failed_instruments: Vec<String>` to `CandleFetchSummary`
  - Use `registry.get(security_id).display_label` for symbol name, fall back to `"{security_id} ({segment})"`
  - After all retry waves, collect from `pending_indices` (cap at 50)
  - Add `persist_failures: usize` to `CandleFetchSummary`
  - Tests: test_candle_fetch_summary_includes_failed_names, test_failed_names_capped_at_max

- [ ] 4. Fix QuestDB persist failure counting
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Both `persist_intraday_candles` and `persist_daily_candles`:
    - On `append_candle` error: do NOT increment `count`, increment `persist_failures` instead
    - Return `(count, persist_failures)` tuple
  - Caller accumulates `persist_failures` into `CandleFetchSummary`
  - Tests: test_persist_failure_not_counted_as_success

- [ ] 5. Enhance Telegram notifications with precise violation details
  - Files: crates/core/src/notification/events.rs
  - **A. `HistoricalFetchFailed`** — add:
    - `failed_instruments: Vec<String>` (symbol names, up to 10 shown + "+N more")
    - `persist_failures: usize`
  - **B. `HistoricalFetchComplete`** — add:
    - `persist_failures: usize` (shown as warning if > 0)
  - **C. `CandleVerificationFailed`** — add:
    - `data_violations: usize`
    - `timestamp_violations: usize`
    - `ohlc_details: Vec<String>` (pre-formatted lines like "RELIANCE (NSE_EQ) 1m @ 10:15 IST — H=2440.0 < L=2450.0")
    - `data_details: Vec<String>` (pre-formatted lines)
    - `timestamp_details: Vec<String>` (pre-formatted lines)
  - **D. `CandleVerificationPassed`** — add:
    - `data_violations: usize`
    - `timestamp_violations: usize`
  - Tests: all message format tests (see item 7)

- [ ] 6. Wire new fields through main.rs
  - Files: crates/app/src/main.rs
  - Pass `InstrumentRegistry` to `verify_candle_integrity()` (new param)
  - Format `ViolationDetail` → pre-formatted strings for Telegram
  - Wire `summary.failed_instruments` + `summary.persist_failures` to notification events
  - Wire `verify_report.ohlc_details` / `data_details` / `timestamp_details` to events
  - Same for post-market re-fetch path
  - Tests: (covered by notification tests)

- [ ] 7. Add comprehensive tests
  - Files: cross_verify.rs, candle_fetcher.rs, events.rs
  - **cross_verify.rs:**
    - test_violation_detail_struct
    - test_cross_verify_report_new_fields_default
    - test_zero_instruments_fails_verification
    - test_passed_requires_zero_all_violations
  - **candle_fetcher.rs:**
    - test_candle_fetch_summary_includes_failed_names
    - test_persist_failure_not_counted_as_success
    - test_failed_names_capped_at_max
  - **events.rs:**
    - test_historical_fetch_failed_shows_instrument_names
    - test_historical_fetch_failed_shows_persist_errors
    - test_historical_fetch_failed_truncates_long_list
    - test_candle_verification_failed_shows_ohlc_details
    - test_candle_verification_failed_shows_data_details
    - test_candle_verification_failed_shows_timestamp_details
    - test_candle_verification_passed_shows_warnings_if_any
    - test_historical_fetch_complete_shows_persist_warnings

## Telegram Message Mockups (Exact Format)

### Fetch — Partial Failure
```
Historical candle fetch — partial failure
Fetched: 229
Failed: 3
Candles: 172,125
Persist errors: 0

Failed instruments:
• RELIANCE (NSE_EQ)
• NIFTY50 (IDX_I)
• BANKNIFTY (IDX_I)
```

### Fetch — Success with Persist Warnings
```
Historical candles OK
Fetched: 232
Skipped: 1050
Candles: 187,458
Persist errors: 42
```

### Verification — FAILED with Precise Details
```
Candle verification FAILED
Checked: 232 | Gaps: 3

OHLC violations (2):
• RELIANCE (NSE_EQ) 1m @ 2026-03-18 10:15 IST
  H=2440.0 < L=2450.0
• TCS (NSE_EQ) 5m @ 2026-03-18 11:30 IST
  H=3500.0 < L=3520.0

Data violations (5):
• INFY (NSE_EQ) 1m @ 2026-03-18 09:15 IST
  O=0.0 H=1580.5 L=1575.0 C=1578.0
• NIFTY50 (IDX_I) 1m @ 2026-03-18 14:20 IST
  O=-1.0 H=23500.0 L=23480.0 C=23490.0
... +3 more

Timestamp violations (8):
• HDFCBANK (NSE_EQ) 1m @ 2026-03-18 09:14 IST
  outside 09:15-15:29 market hours
• SBIN (NSE_EQ) 1m @ 2026-03-18 15:31 IST
  outside 09:15-15:29 market hours
... +6 more

Timeframes:
1m: 85,125 (229 inst)
5m: 17,250 (232 inst)
15m: 5,750 (232 inst)
60m: 1,437 (232 inst)
1d: 232 (232 inst)
```

### Verification — FAILED with Zero Instruments
```
Candle verification FAILED
Checked: 0

No instrument data found — fetch may have completely failed
```

### Verification — PASSED (clean)
```
Candle verification OK
Instruments: 232
Total candles: 187,500

Timeframes:
1m: 86,250 (232 inst)
5m: 17,250 (232 inst)
15m: 5,750 (232 inst)
60m: 1,437 (232 inst)
1d: 232 (232 inst)

Checks: OHLC ✓ | Data ✓ | Timestamps ✓
```

### Verification — PASSED but with minor warnings
```
Candle verification OK
Instruments: 232
Total candles: 187,500

Timeframes:
1m: 86,250 (232 inst)
5m: 17,250 (232 inst)
15m: 5,750 (232 inst)
60m: 1,437 (232 inst)
1d: 232 (232 inst)

Data violations: 2 (non-blocking)
Timestamp violations: 1 (non-blocking)
```

## Scenarios

| # | Scenario | What Telegram Shows |
|---|----------|-------------------|
| 1 | 3 instruments fail all retries | Names: RELIANCE, NIFTY50, BANKNIFTY with segments |
| 2 | QuestDB down during persist | "Persist errors: 42" on fetch notification |
| 3 | Empty DB (0 instruments) | "Checked: 0 — No instrument data found" — FAILS |
| 4 | high < low for 2 candles | "RELIANCE 1m @ 10:15 IST — H=2440.0 < L=2450.0" with exact values |
| 5 | open=0.0 for 5 candles | "INFY 1m @ 09:15 IST — O=0.0 H=1580.5 L=1575.0 C=1578.0" |
| 6 | Candle at 09:14 IST | "HDFCBANK 1m @ 09:14 IST — outside 09:15-15:29 market hours" |
| 7 | All good | "Checks: OHLC ✓ | Data ✓ | Timestamps ✓" |
| 8 | Token expires mid-fetch | Symbol in "Failed instruments" list |

## Files Modified (4 source files + tests inline)

| File | Changes |
|------|---------|
| `crates/core/src/historical/cross_verify.rs` | +ViolationDetail, +3 detail queries, +3 count queries, +registry param, fix pass logic |
| `crates/core/src/historical/candle_fetcher.rs` | +failed_instruments, +persist_failures, fix count bug |
| `crates/core/src/notification/events.rs` | +fields on 4 events, precise message formatting |
| `crates/app/src/main.rs` | Wire registry + new fields (initial + post-market) |

## Principles Check

| Principle | Compliance |
|-----------|-----------|
| Zero allocation on hot path | YES — all cold path (boot + background) |
| O(1) or fail at compile time | YES — SQL queries O(n) in QuestDB, capped LIMIT 20 |
| Every version pinned | YES — no new dependencies |
