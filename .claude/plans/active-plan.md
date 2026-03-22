# Implementation Plan: Security Fixes + Mock Calendar Display

**Status:** APPROVED
**Date:** 2026-03-22
**Approved by:** Parthiban

## Plan Items

- [ ] Item 1: Fix HIGH — Add loop bound to `next_trading_day()` (max 14 iterations)
  - Files: crates/common/src/trading_calendar.rs
  - Tests: test_next_trading_day_bounded_loop, existing tests unchanged

- [ ] Item 2: Fix HIGH — Add `config/local.toml` to `.gitignore`
  - Files: .gitignore
  - Tests: N/A (git config)

- [ ] Item 3: Add mock trading dates to `all_entries()` + Grafana display
  - Files: crates/common/src/trading_calendar.rs (HolidayInfo + all_entries), crates/storage/src/calendar_persistence.rs (classify_holiday_type), deploy/docker/grafana/dashboards/market-data.json (color mapping + title)
  - Tests: test_all_entries_includes_mock_trading, test_classify_holiday_type_mock

- [ ] Item 4: Fix MEDIUM — Document fast-boot mock exclusion intent
  - Files: crates/app/src/main.rs
  - Tests: N/A (comment only)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | next_trading_day with 15 consecutive holidays | Returns error, not infinite loop |
| 2 | Grafana dashboard on mock trading Saturday | Shows "Mock Trading Session" in orange/blue |
| 3 | Daily instrument load with expired contracts | Expired filtered at build, never deleted from QuestDB |
| 4 | Security ID reused across underlyings | Delta detector fires SecurityIdReused event |
