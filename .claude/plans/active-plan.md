# Implementation Plan: Weekend/Holiday Boot Fix + NSE Mock Trading Calendar

**Status:** VERIFIED
**Date:** 2026-03-22
**Approved by:** Parthiban

## Context

When manually starting the app on weekends/holidays, `load_or_build_instruments()` uses a
purely time-based `is_within_build_window()` check. On Sunday 11 AM, this returns TRUE
(11:00 is within 09:00–15:30), so the loader takes the cache-only path instead of
downloading fresh instruments. This was designed to protect live trading from download
disruption, but on non-trading days there's no live market to protect.

Additionally, NSE conducts monthly mock trading sessions on Saturdays. The system has no
awareness of these dates.

## Plan Items

- [x] Item 1: Pass `is_trading_day` into `load_or_build_instruments()` so non-trading days always download fresh
  - Files: crates/core/src/instrument/instrument_loader.rs, crates/app/src/main.rs
  - Tests: test_non_trading_day_bypasses_build_window, test_trading_day_respects_build_window
  - Logic: Add `is_trading_day: bool` parameter. When `!is_trading_day`, always take the download-fresh path (same as outside-market-hours), regardless of time-of-day.

- [x] Item 2: Add `nse_mock_trading_dates` config section to TradingConfig and base.toml
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_mock_trading_config_deserialized
  - Logic: New `Vec<NseHolidayEntry>` field with 2026 NSE mock trading dates. Same format as holidays/muhurat.

- [x] Item 3: Add `is_mock_trading_day()` to TradingCalendar
  - Files: crates/common/src/trading_calendar.rs
  - Tests: test_mock_trading_day_detected, test_non_mock_saturday_not_mock_day, test_mock_trading_day_is_not_regular_trading_day
  - Logic: New `mock_trading_dates: HashSet<NaiveDate>` + `is_mock_trading_day()` + `is_mock_trading_today()`. Mock trading days are NOT regular trading days (no real orders), but the system should be aware of them for operational context.

- [x] Item 4: Update `load_instruments()` wrapper in main.rs to pass `is_trading_day`
  - Files: crates/app/src/main.rs
  - Tests: (covered by Item 1 tests — the wrapper just passes the flag through)
  - Logic: Both fast boot and slow boot paths need the trading calendar's `is_trading_day_today()` result passed to the instrument loader.

- [x] Item 5: Log mock trading day status at boot
  - Files: crates/app/src/main.rs
  - Tests: (log output — no unit test needed, verified by code review)
  - Logic: At lines 214–224 where trading day status is logged, also log `is_mock_trading_day` status. On mock trading Saturdays, log INFO that it's a mock trading session with compressed hours.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Sunday 11 AM manual start | Downloads fresh instruments (not cache), full boot, all components loaded |
| 2 | Sunday 8 AM manual start | Downloads fresh instruments (same as before — already correct) |
| 3 | Saturday 10 AM (mock trading day) | Downloads fresh instruments, logs "mock trading session" |
| 4 | Saturday 10 AM (non-mock Saturday) | Downloads fresh instruments, logs "non-trading day" |
| 5 | Monday 10 AM (trading day, market hours) | Cache-first path (unchanged — protects live trading) |
| 6 | Monday 8 AM (trading day, pre-market) | Downloads fresh (unchanged — outside build window) |
| 7 | Holiday 11 AM (e.g., Republic Day) | Downloads fresh instruments (not cache) |
| 8 | Config with mock dates parses correctly | 12 mock dates from 2026 calendar loaded |

## Non-Goals

- NOT adding automatic start on mock trading Saturdays (manual start only)
- NOT sending real orders on mock trading days (dry_run enforced until June)
- NOT changing WebSocket behavior on weekends (connects regardless, just no data on non-mock days)
