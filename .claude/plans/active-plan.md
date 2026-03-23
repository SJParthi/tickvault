# Implementation Plan: Fix Greeks Tables — IST Timestamps, Symbol Names, Verification Logic

**Status:** DRAFT
**Date:** 2026-03-23
**Approved by:** pending

## Context

All 4 Greeks-related QuestDB tables have issues:
1. Timestamps are UTC instead of IST (missing +5:30 offset)
2. symbol_name columns are always empty
3. Greeks verification shows MISMATCH for everything due to absolute thresholds on unbounded values
4. time-to-expiry uses UTC date instead of IST (off by 1 day near midnight)
5. No mechanical enforcement rule to prevent future regression

## Plan Items

- [ ] P1: Fix IST timestamp offset for all 4 tables
  - Files: crates/app/src/greeks_pipeline.rs
  - Change line 128: add `IST_UTC_OFFSET_NANOS` to `now_nanos` (same pattern as `received_at` in tick_persistence.rs:438)
  - Import: `use dhan_live_trader_common::constants::IST_UTC_OFFSET_NANOS;`
  - Tests: test_critical_greeks_timestamp_includes_ist_offset (source code scan)

- [ ] P2: Fix IST-based today date for time-to-expiry
  - Files: crates/app/src/greeks_pipeline.rs
  - Change line 129: use IST-based date instead of UTC
  - Import: `use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS_I64;`
  - Tests: test_today_uses_ist_date

- [ ] P3: Populate symbol_name from available data
  - Files: crates/app/src/greeks_pipeline.rs
  - Change line 313: construct display name `"{UNDERLYING} {STRIKE:.0} {CE/PE}"` (e.g., "NIFTY 23000 CE")
  - Cold path (every 60s) so format!() is fine
  - Tests: test_symbol_name_constructed

- [ ] P4: Fix verification thresholds — percentage-based for unbounded Greeks
  - Files: crates/app/src/greeks_pipeline.rs
  - Changes to classify_match():
    - Delta: absolute 0.05 (bounded [0,1])
    - Gamma: absolute 0.005 (small values ~0.001)
    - Theta: 25% relative (unbounded, model-dependent)
    - Vega: 25% relative (unbounded, model-dependent)
    - IV: 15% relative (model-parameter-dependent)
  - Tests: test_classify_match_percentage_theta, test_classify_match_percentage_vega, test_classify_match_percentage_iv, test_classify_match_mismatch_large_diff

- [ ] P5: Add mechanical enforcement rule for IST timestamps on Greeks tables
  - Files: .claude/rules/project/data-integrity.md
  - Add "Greeks Pipeline Timestamp Rule" section documenting:
    - All 4 tables MUST use `Utc::now() + IST_UTC_OFFSET_NANOS`
    - Today's date MUST use IST timezone
    - Mechanical test enforces this
  - Tests: (enforcement rule is documentation, mechanical test from P1 covers it)

- [ ] P6: Update tests for all fixes
  - Files: crates/app/src/greeks_pipeline.rs (test module)
  - Update existing: test_classify_match_within_threshold, test_classify_match_partial, test_classify_match_mismatch
  - Add new: test_critical_greeks_timestamp_includes_ist_offset, test_symbol_name_format, test_classify_match_percentage_theta, test_classify_match_percentage_vega, test_classify_match_percentage_iv

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB timestamps after fix | Show IST time (14:34 instead of 09:04) |
| 2 | symbol_name column | Shows "NIFTY 23000 CE" format |
| 3 | ATM theta (-3.5 vs -3.7) | MATCH (5.7% diff < 25%) |
| 4 | Deep ITM theta (-655 vs -540) | MATCH (21% diff < 25%) |
| 5 | Vega close (26.98 vs 28.21) | MATCH (4.4% diff < 25%) |
| 6 | Vega far (3.23 vs 2.303) | MISMATCH (40% diff > 25%) |
| 7 | Delta close (0.54 vs 0.53) | MATCH (0.01 abs < 0.05) |
| 8 | Pipeline at 12:30 AM IST | today = current IST date |
