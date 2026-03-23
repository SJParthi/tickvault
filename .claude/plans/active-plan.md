# Implementation Plan: Fix Greeks Tables — IST, Symbols, Exact Parameter Calibration

**Status:** DRAFT
**Date:** 2026-03-23
**Approved by:** pending

## Context

All 4 Greeks-related QuestDB tables have issues:
1. Timestamps are UTC instead of IST (missing +5:30 offset)
2. symbol_name columns are always empty
3. Greeks verification shows MISMATCH — our BS parameters don't match Dhan's
4. time-to-expiry uses UTC date instead of IST (off by 1 day near midnight)
5. No mechanical enforcement rule to prevent future regression

**Core principle**: Our Greeks MUST match Dhan's EXACTLY (within f64 epsilon).
No percentage tolerance. Without exact match, backtesting is unreliable.

## Calibration Strategy

To achieve exact match, we isolate two problems:

**Step A — Formula validation**: Use Dhan's IV directly → compute Greeks with our BS formulas.
If these match Dhan's Greeks with the RIGHT parameters, our formulas are correct.

**Step B — Parameter calibration**: Grid search over (r, q, day_count) to find the exact
combination that makes our Greeks match Dhan's across ALL strikes simultaneously.

Most likely Dhan parameters (to test first):
- `risk_free_rate`: 0.0, 0.05, 0.065, 0.068, 0.07
- `dividend_yield`: 0.0, 0.005, 0.01, 0.012
- `day_count`: 365, 365.25, 252

Once found, update config defaults → our IV solver also produces matching results.

## Plan Items

- [ ] P1: Fix IST timestamp offset for all 4 tables
  - Files: crates/app/src/greeks_pipeline.rs
  - Change line 128: add `IST_UTC_OFFSET_NANOS` to `now_nanos`
  - Import: `use dhan_live_trader_common::constants::IST_UTC_OFFSET_NANOS;`
  - Tests: test_critical_greeks_timestamp_includes_ist_offset

- [ ] P2: Fix IST-based today date for time-to-expiry
  - Files: crates/app/src/greeks_pipeline.rs
  - Change line 129: use IST-based date instead of UTC
  - Import: `use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS_I64;`
  - Tests: test_today_uses_ist_date

- [ ] P3: Populate symbol_name from available data
  - Files: crates/app/src/greeks_pipeline.rs
  - Change line 313: construct `"{UNDERLYING} {STRIKE:.0} {CE/PE}"` (e.g., "NIFTY 23000 CE")
  - Cold path (every 60s) so format!() is fine
  - Tests: test_symbol_name_constructed

- [ ] P4: Add dual-mode verification with Dhan IV passthrough
  - Files: crates/app/src/greeks_pipeline.rs, crates/trading/src/greeks/black_scholes.rs
  - Add `compute_greeks_from_iv()` — takes IV directly (no solver), computes delta/gamma/theta/vega
  - In verification: compute Greeks TWICE:
    1. Our IV solver → our Greeks (existing flow)
    2. Dhan's IV → our formulas → "calibrated Greeks" (new)
  - Compare calibrated Greeks vs Dhan's → this isolates formula correctness from parameter error
  - Add columns to greeks_verification: `calibrated_delta`, `calibrated_theta`, etc.
  - Match status: EXACT if calibrated Greeks match within f64 epsilon
  - Tests: test_compute_greeks_from_iv_matches_full, test_dhan_iv_passthrough_verification

- [ ] P5: Parameter calibration — find Dhan's exact r, q, day_count
  - Files: crates/trading/src/greeks/calibration.rs (NEW), crates/trading/src/greeks/mod.rs
  - Add `calibrate_parameters()` function:
    - Input: Vec of (spot, strike, time_days, dhan_iv, dhan_delta, dhan_gamma, dhan_theta, dhan_vega)
    - Grid search over r ∈ {0.0, 0.05, 0.065, 0.068, 0.07}, q ∈ {0.0, 0.005, 0.01, 0.012}, daycount ∈ {365, 365.25, 252}
    - Score = sum of squared differences across all strikes
    - Output: best (r, q, daycount) combination
  - Run calibration on first cycle, log results, update config if needed
  - Tests: test_calibrate_known_parameters, test_calibrate_zero_error_with_correct_params

- [ ] P6: Update config defaults after calibration confirms Dhan's parameters
  - Files: crates/common/src/config.rs
  - Update `default_risk_free_rate()` and `default_dividend_yield()` to match Dhan's exact values
  - Update `compute_time_to_expiry()` day count to match Dhan's convention
  - Tests: existing config tests + calibration round-trip test

- [ ] P7: Fix classify_match for exact-match verification
  - Files: crates/app/src/greeks_pipeline.rs
  - MATCH = all Greeks within f64 epsilon (1e-6) when using Dhan's IV + calibrated params
  - PARAM_DIFF = calibrated matches but our IV solver produces different IV (expected if params differ)
  - MISMATCH = even calibrated Greeks don't match (formula bug or data issue)
  - Tests: test_classify_exact_match, test_classify_param_diff, test_classify_formula_mismatch

- [ ] P8: Add mechanical enforcement rule for IST timestamps
  - Files: .claude/rules/project/data-integrity.md
  - Add "Greeks Pipeline Timestamp Rule" section
  - Tests: test_critical_greeks_timestamp_includes_ist_offset (source code scan)

- [ ] P9: Add/update all tests, build, verify
  - Files: crates/app/src/greeks_pipeline.rs, crates/trading/src/greeks/calibration.rs
  - All new tests listed above
  - cargo build + cargo test --workspace
  - Tests: full test suite passes

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB timestamps | Show IST time (14:34 not 09:04) |
| 2 | symbol_name column | Shows "NIFTY 23000 CE" format |
| 3 | Dhan IV → our formulas (correct params) | EXACT match (diff < 1e-6) |
| 4 | Our IV solver (calibrated params) | EXACT match (diff < 1e-6) |
| 5 | Our IV solver (wrong params) | PARAM_DIFF status (IV differs, but calibrated match is exact) |
| 6 | Calibration grid search | Finds exact (r, q, daycount) matching Dhan |
| 7 | Pipeline at 12:30 AM IST | today = current IST date |
| 8 | Backtesting confidence | Exact match → safe to use our computation |
