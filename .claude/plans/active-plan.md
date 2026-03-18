# Implementation Plan: Historical Candle Fetch Hardening — Precise Diagnostics

**Status:** VERIFIED
**Date:** 2026-03-18
**Approved by:** Parthiban

## Problem Statement

The historical candle system silently loses data and provides insufficient diagnostic
information. Telegram notifications must be **self-contained diagnostic reports** showing
exactly WHICH symbol, at WHICH timestamp, with WHICH OHLCV values are wrong — so you
can diagnose the issue just by reading the notification.

Additionally, there is **no cross-match between historical API candles and live WebSocket
candles**. If WebSocket drops ticks, the live materialized view candles will silently
differ from the canonical API data — and nobody checks.

### Gaps Found

| # | Gap | Severity | Current | Required |
|---|-----|----------|---------|----------|
| 1 | Cross-verify only checks `high < low` count | CRITICAL | "OHLC violations: 2" — which symbols? | Show symbol, timestamp, actual values |
| 2 | `instruments_checked == 0` passes | CRITICAL | Empty DB = OK | Must FAIL |
| 3 | QuestDB append failures silent | HIGH | Candle lost, count inflated | Track + alert via Telegram |
| 4 | Telegram shows only failed COUNT | HIGH | "Failed: 3" | Show symbol names + segments |
| 5 | No timestamp bounds check | HIGH | Out-of-hours candles undetected | Validate 09:15-15:29 IST |
| 6 | No OHLCV value details | HIGH | Just a count | Show exact values per violation |
| 7 | **No Historical vs Live cross-match** | **CRITICAL** | Live data drift undetected | Compare OHLCV per instrument per timestamp |

## Architecture

### Symbol Name Resolution
```
QuestDB query → security_id + timestamp + OHLCV
    ↓
InstrumentRegistry.get(security_id) → display_label ("RELIANCE", "NIFTY50")
    ↓
ViolationDetail / CrossMatchDetail with symbol name
    ↓
Telegram message with precise diagnostics
```

### Historical vs Live Cross-Match Data Flow
```
historical_candles table (Dhan REST API)
    ↓  LEFT JOIN on (security_id, ts, segment)
candles_1m / candles_5m / candles_15m / candles_1h / candles_1d (live MV from ticks)
    ↓
Detect: missing live candles, OHLCV mismatches, volume drift
    ↓
CrossMatchReport with per-instrument per-timestamp details
    ↓
Telegram: "RELIANCE 1m @ 10:15 IST — Hist: O=2450 H=2465 / Live: O=2450 H=2463.5 / Diff: H(-1.5)"
```

### Timeframe Mapping (Historical → Materialized View)
```
Historical "1m"  → candles_1m
Historical "5m"  → candles_5m
Historical "15m" → candles_15m
Historical "60m" → candles_1h    (note: different naming!)
Historical "1d"  → candles_1d
```

## New Types

```rust
/// A single candle that failed cross-verification, with full diagnostic context.
pub struct ViolationDetail {
    pub symbol: String,         // "RELIANCE", "NIFTY50"
    pub segment: String,        // "NSE_EQ", "IDX_I"
    pub timeframe: String,      // "1m", "5m", "1d"
    pub timestamp_ist: String,  // "2026-03-18 10:15"
    pub violation: String,      // "high < low", "open <= 0", "outside market hours"
    pub values: String,         // "O=0.0 H=2450.5 L=2440.0 C=2445.0"
}

/// A single candle mismatch between historical API data and live materialized view.
pub struct CrossMatchMismatch {
    pub symbol: String,         // "RELIANCE"
    pub segment: String,        // "NSE_EQ"
    pub timeframe: String,      // "1m"
    pub timestamp_ist: String,  // "2026-03-18 10:15"
    pub mismatch_type: String,  // "price_diff", "missing_live", "missing_historical"
    pub hist_values: String,    // "O=2450.0 H=2465.0 L=2448.0 C=2460.0 V=125000"
    pub live_values: String,    // "O=2450.0 H=2463.5 L=2448.0 C=2460.0 V=118500"
    pub diff_summary: String,   // "H(-1.5) V(-6500)"
}

/// Summary of historical vs live candle cross-match.
pub struct CrossMatchReport {
    pub timeframes_checked: usize,       // 5 (1m, 5m, 15m, 60m, 1d)
    pub candles_compared: usize,         // total candles matched across all timeframes
    pub mismatches: usize,               // total count of mismatched candles
    pub missing_live: usize,             // historical exists but live doesn't
    pub missing_historical: usize,       // live exists but historical doesn't
    pub mismatch_details: Vec<CrossMatchMismatch>, // capped at 20
    pub passed: bool,                    // true if mismatches within tolerance
}
```

## Plan Items

- [x] 1. Add `ViolationDetail` struct and detailed SQL queries to cross-verify
  - Files: crates/core/src/historical/cross_verify.rs
  - Changes:
    - Add `ViolationDetail` struct
    - Add `ohlc_details: Vec<ViolationDetail>` to `CrossVerificationReport` (capped at 20)
    - Add `data_details: Vec<ViolationDetail>` to `CrossVerificationReport` (capped at 20)
    - Add `timestamp_details: Vec<ViolationDetail>` to `CrossVerificationReport` (capped at 20)
    - Add `data_violations: usize` count field
    - Add `timestamp_violations: usize` count field
    - **OHLC detail query** — returns actual rows:
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
        AND (open <= 0 OR high <= 0 OR low <= 0 OR close <= 0)
      LIMIT 20
      ```
    - **Timestamp violation detail query** (intraday outside 09:15-15:29 IST):
      QuestDB stores IST-as-UTC. Market hours: 03:45 UTC to 09:59 UTC (= 09:15-15:29 IST).
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
    - **Count queries** for totals (detail queries are LIMIT 20):
      ```sql
      SELECT count() FROM historical_candles WHERE ... AND (open <= 0 OR ...)
      SELECT count() FROM historical_candles WHERE ... AND (timestamp outside hours)
      ```
    - Pass `InstrumentRegistry` to `verify_candle_integrity()` for security_id → symbol lookup
    - Update pass/fail: `passed = gaps==0 && ohlc==0 && data==0 && timestamp==0 && checked>0`
  - Tests: test_violation_detail_struct, test_cross_verify_report_new_fields

- [x] 2. Fix zero-instruments-passes-verification bug
  - Files: crates/core/src/historical/cross_verify.rs
  - Remove `|| instruments_checked == 0` from pass condition
  - `instruments_checked == 0` → `passed = false`
  - Tests: test_zero_instruments_fails_verification

- [x] 3. Track failed instrument names in `CandleFetchSummary`
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Add `failed_instruments: Vec<String>` to `CandleFetchSummary`
  - Use `registry.get(security_id).display_label` for symbol name, fall back to `"{security_id} ({segment})"`
  - After all retry waves, collect from `pending_indices` (cap at 50)
  - Add `persist_failures: usize` to `CandleFetchSummary`
  - Tests: test_candle_fetch_summary_includes_failed_names, test_failed_names_capped_at_max

- [x] 4. Fix QuestDB persist failure counting
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Both `persist_intraday_candles` and `persist_daily_candles`:
    - On `append_candle` error: do NOT increment `count`, increment `persist_failures` instead
    - Return `(count, persist_failures)` tuple
  - Caller accumulates `persist_failures` into `CandleFetchSummary`
  - Tests: test_persist_failure_not_counted_as_success

- [x] 5. Enhance Telegram notifications with precise violation details
  - Files: crates/core/src/notification/events.rs
  - **A. `HistoricalFetchFailed`** — add:
    - `failed_instruments: Vec<String>` (up to 10 shown + "+N more")
    - `persist_failures: usize`
  - **B. `HistoricalFetchComplete`** — add:
    - `persist_failures: usize` (shown as warning if > 0)
  - **C. `CandleVerificationFailed`** — add:
    - `data_violations: usize`
    - `timestamp_violations: usize`
    - `ohlc_details: Vec<String>` (pre-formatted violation lines)
    - `data_details: Vec<String>` (pre-formatted)
    - `timestamp_details: Vec<String>` (pre-formatted)
  - **D. `CandleVerificationPassed`** — add:
    - `data_violations: usize`
    - `timestamp_violations: usize`
  - Tests: all message format tests (see item 9)

- [x] 6. Wire new fields through main.rs for historical verification
  - Files: crates/app/src/main.rs
  - Pass `InstrumentRegistry` to `verify_candle_integrity()` (new param)
  - Format `ViolationDetail` → pre-formatted strings for Telegram
  - Wire `summary.failed_instruments` + `summary.persist_failures` to events
  - Wire `verify_report` detail fields to events
  - Same for post-market re-fetch path
  - Tests: (covered by notification tests)

- [x] 7. Add Historical vs Live candle cross-match verification
  - Files: crates/core/src/historical/cross_verify.rs, crates/core/src/notification/events.rs, crates/app/src/main.rs
  - **New function:** `pub async fn cross_match_historical_vs_live(questdb_config, registry) -> CrossMatchReport`
  - **For each overlapping timeframe (5 queries):**
    - 1m: `historical_candles WHERE timeframe='1m'` LEFT JOIN `candles_1m`
    - 5m: `historical_candles WHERE timeframe='5m'` LEFT JOIN `candles_5m`
    - 15m: `historical_candles WHERE timeframe='15m'` LEFT JOIN `candles_15m`
    - 60m: `historical_candles WHERE timeframe='60m'` LEFT JOIN `candles_1h`
    - 1d: `historical_candles WHERE timeframe='1d'` LEFT JOIN `candles_1d`
  - **Mismatch detection SQL (per timeframe, e.g., 1m):**
    ```sql
    SELECT
      h.security_id, h.segment, h.ts,
      h.open as h_open, h.high as h_high, h.low as h_low, h.close as h_close, h.volume as h_vol,
      m.open as m_open, m.high as m_high, m.low as m_low, m.close as m_close, m.volume as m_vol
    FROM historical_candles h
    LEFT JOIN candles_1m m
      ON h.security_id = m.security_id AND h.ts = m.ts AND h.segment = m.segment
    WHERE h.timeframe = '1m'
      AND h.ts > dateadd('d', -1, now())
      AND (m.open IS NULL
           OR abs(h.open - m.open) > 0.01
           OR abs(h.high - m.high) > 0.01
           OR abs(h.low - m.low) > 0.01
           OR abs(h.close - m.close) > 0.01)
    LIMIT 20
    ```
  - **Count query per timeframe:**
    ```sql
    SELECT count() FROM historical_candles h
    LEFT JOIN candles_1m m ON h.security_id = m.security_id AND h.ts = m.ts AND h.segment = m.segment
    WHERE h.timeframe = '1m' AND h.ts > dateadd('d', -1, now())
      AND (m.open IS NULL OR abs(h.open - m.open) > 0.01 OR ...)
    ```
  - **Missing live detection:** `m.open IS NULL` means historical candle exists but live didn't aggregate it (WebSocket missed ticks for that minute)
  - **Price tolerance:** 0.01 absolute (handles f32→f64 rounding; any real price diff > 0.01 is meaningful)
  - **Runs ONLY after post-market re-fetch** (both datasets complete for the day)
  - **New notification event: `CandleCrossMatchPassed` / `CandleCrossMatchFailed`**
  - Tests: test_cross_match_report_struct, test_cross_match_mismatch_struct, test_cross_match_timeframe_mapping

- [x] 8. Add cross-match Telegram notification events
  - Files: crates/core/src/notification/events.rs
  - **`CandleCrossMatchPassed`:**
    ```
    Historical vs Live cross-match OK
    Timeframes: 5 | Candles compared: 187,500
    All OHLCV values match within tolerance (0.01)
    ```
  - **`CandleCrossMatchFailed`:**
    ```
    Historical vs Live cross-match FAILED
    Compared: 187,500 | Mismatches: 12
    Missing live: 8 | Missing historical: 0

    Mismatches:
    • RELIANCE (NSE_EQ) 1m @ 2026-03-18 10:15 IST
      Hist: O=2450.0 H=2465.0 L=2448.0 C=2460.0 V=125000
      Live: O=2450.0 H=2463.5 L=2448.0 C=2460.0 V=118500
      Diff: H(-1.5) V(-6500)

    • TCS (NSE_EQ) 1m @ 2026-03-18 11:30 IST
      Hist: O=3520.0 H=3535.0 L=3518.0 C=3530.0 V=45000
      Live: [MISSING — no live data for this candle]

    • NIFTY50 (IDX_I) 5m @ 2026-03-18 14:00 IST
      Hist: O=23480.0 H=23510.0 L=23475.0 C=23505.0 V=0
      Live: O=23480.0 H=23508.5 L=23475.0 C=23504.0 V=0
      Diff: H(-1.5) C(-1.0)
    ... +9 more
    ```
  - Severity: `CandleCrossMatchFailed` = High (Telegram + SNS)
  - Tests: test_cross_match_failed_shows_details, test_cross_match_passed_message, test_cross_match_missing_live_format

- [x] 9. Add comprehensive tests for all changes
  - Files: cross_verify.rs, candle_fetcher.rs, events.rs
  - **cross_verify.rs:**
    - test_violation_detail_struct
    - test_cross_verify_report_new_fields_default
    - test_zero_instruments_fails_verification
    - test_passed_requires_zero_all_violations
    - test_cross_match_report_struct
    - test_cross_match_mismatch_struct
    - test_cross_match_timeframe_mapping
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
    - test_cross_match_failed_shows_details
    - test_cross_match_passed_message
    - test_cross_match_missing_live_format
    - test_cross_match_failed_is_high_severity

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

### Cross-Match — FAILED (WebSocket missed ticks)
```
Historical vs Live cross-match FAILED
Compared: 187,500 | Mismatches: 12
Missing live: 8 | Missing historical: 0

Mismatches:
• RELIANCE (NSE_EQ) 1m @ 2026-03-18 10:15 IST
  Hist: O=2450.0 H=2465.0 L=2448.0 C=2460.0 V=125000
  Live: O=2450.0 H=2463.5 L=2448.0 C=2460.0 V=118500
  Diff: H(-1.5) V(-6500)

• TCS (NSE_EQ) 1m @ 2026-03-18 11:30 IST
  Hist: O=3520.0 H=3535.0 L=3518.0 C=3530.0 V=45000
  Live: [MISSING — no live data for this candle]

• NIFTY50 (IDX_I) 5m @ 2026-03-18 14:00 IST
  Hist: O=23480.0 H=23510.0 L=23475.0 C=23505.0 V=0
  Live: O=23480.0 H=23508.5 L=23475.0 C=23504.0 V=0
  Diff: H(-1.5) C(-1.0)
... +9 more
```

### Cross-Match — PASSED (all good)
```
Historical vs Live cross-match OK
Timeframes: 5 | Candles compared: 187,500
All OHLCV values match within tolerance (0.01)
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
| 7 | All good | "Checks: OHLC ✓ | Data ✓ | Timestamps ✓" + "Cross-match OK" |
| 8 | Token expires mid-fetch | Symbol in "Failed instruments" list |
| 9 | WebSocket dropped 5min | Cross-match: "MISSING — no live data" for those 5 min of candles |
| 10 | Live tick reordering | Cross-match: "Diff: H(-0.5)" small price difference flagged |
| 11 | Full WS disconnect during session | Cross-match: "Missing live: 200+" — massive gap detected |

## Execution Order

1. Items 1-6: Historical candle verification hardening (can implement together)
2. Item 7-8: Cross-match (separate feature, depends on items 1-6)
3. Item 9: Tests for everything

## Files Modified (4 source files + tests inline)

| File | Changes |
|------|---------|
| `crates/core/src/historical/cross_verify.rs` | +ViolationDetail, +CrossMatchMismatch, +CrossMatchReport, +detail queries, +cross-match queries, +registry param, fix pass logic |
| `crates/core/src/historical/candle_fetcher.rs` | +failed_instruments, +persist_failures, fix count bug |
| `crates/core/src/notification/events.rs` | +fields on 4 events, +2 new events (CrossMatch), precise formatting |
| `crates/app/src/main.rs` | Wire registry + new fields + cross-match call (post-market only) |

## Principles Check

| Principle | Compliance |
|-----------|-----------|
| Zero allocation on hot path | YES — all cold path (boot + background + post-market) |
| O(1) or fail at compile time | YES — SQL queries O(n) in QuestDB, capped LIMIT 20 |
| Every version pinned | YES — no new dependencies |
