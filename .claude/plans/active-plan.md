# Implementation Plan: Exhaustive cross-match (09:15→15:30 IST, IDX_I + NSE_EQ)

**Status:** APPROVED
**Date:** 2026-04-21
**Approved by:** Parthiban
**Branch:** `claude/cross-match-exhaustive-coverage`

## Background

Live production bug (2026-04-21 15:47 IST): cross-match reported OK with
116,162 candles compared, zero mismatches, despite the app restarting
mid-day leaving huge gaps in live `candles_*` materialized views.

Three root causes in `crates/core/src/historical/cross_verify.rs`:

1. `determine_cross_match_passed` ignores `missing_live` / `missing_historical`.
2. Detail query has `LIMIT MAX_VIOLATION_DETAILS` → `total_mismatches = details.len()` caps silently.
3. `missing_historical` is declared but never populated — no Pass-B query exists.

## Plan Items

- [ ] A. Window end exclusive (`ts <=` → `ts <`)
  - Files: `crates/core/src/historical/cross_verify.rs`
  - Tests: `test_window_end_is_exclusive`

- [ ] B. Scope filter to IDX_I + NSE_EQ only (reject F&O)
  - Files: `crates/core/src/historical/cross_verify.rs`
  - Tests: `test_scope_filter_contains_idx_i_and_nse_eq`

- [ ] C. Pass-B query populating `missing_historical`
  - Files: `crates/core/src/historical/cross_verify.rs`
  - Tests: `test_missing_historical_pass_is_wired`

- [ ] D. Remove detail-query LIMIT (fetch every violation)
  - Files: `crates/core/src/historical/cross_verify.rs`
  - Tests: `test_detail_query_has_no_limit`

- [ ] E. `determine_cross_match_passed` blocks on ANY hole
  - Files: `crates/core/src/historical/cross_verify.rs`
  - Tests: `test_passed_false_when_missing_live_nonzero`,
            `test_passed_false_when_missing_historical_nonzero`

- [ ] F. Rich Telegram format (emoji + monospace tables)
  - Files: `crates/core/src/notification/events.rs`
  - Tests: `test_cross_match_ok_uses_monospace_pre`,
            `test_cross_match_failed_groups_by_category`

- [ ] G. Telegram chunking for >4000-char messages
  - Files: `crates/core/src/notification/service.rs`
  - Tests: `test_chunk_preserves_order`, `test_chunk_splits_on_newline`,
            `test_chunk_below_threshold_sends_single_message`

- [ ] H. Ratchet guard test file
  - Files: `crates/core/tests/cross_match_exhaustive_guard.rs` (new)
  - Tests: source-scans for Pass-B, scope filter, exclusive window, no LIMIT

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App restarts mid-day, big gap in live | FAILED with per-row listing |
| 2 | Full-day clean, no drift | OK single message |
| 3 | Pre-market / no live data | SKIPPED |
| 4 | 1-candle OHLCV drift | FAILED with exact field + hist→live values |
| 5 | 500+ missing minutes | Header + multiple body Telegram msgs, full list, no truncation |
