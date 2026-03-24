# Implementation Plan: Eagle-Eye Precision Greeks Engine

**Status:** VERIFIED
**Date:** 2026-03-24
**Approved by:** Parthiban

## Context

Deep audit by 8 parallel research agents found 26 issues across our Greeks implementation.
The goal: mathematically bulletproof Greeks that match Dhan/NSE exactly for live use AND
provide theoretically-correct values for backtesting. Every Greek to f64 machine epsilon.

### Root Causes Found
1. **Normal CDF**: Abramowitz & Stegun (7.5e-8 error) vs gold standard Cody (6e-19)
2. **IV Solver**: Newton-Raphson (can fail, 1e-8) vs Jaeckel "Let's Be Rational" (never fails, 1e-16)
3. **Config wrong**: `base.toml` has `r=0.068, q=0.012` but Dhan uses `r=0.10, q=0.0`
4. **Missing Greeks**: Only 4 of 13 computed (missing rho + all second/third order)
5. **No edge case guards**: NaN propagation, expiry-day blackout, intrinsic inconsistency

### Key Research Findings
- **Dhan convention**: r=10%, q=0%, day_count=365, theta=per-calendar-day, vega=per-1%-IV
- **NSE convention**: Same as Dhan (r=10% for displayed IV, ACT/365)
- **Gold standard**: Jaeckel's "Let's Be Rational" + Cody's erfc = machine epsilon for ALL inputs
- **Rust crates**: `implied-vol` v2.0.0 (Jaeckel), `jaeckel` v0.2.0 (Cody erfc + Jaeckel IV)
- **QuantLib reference**: Cody erf, Brent solver (inferior to Jaeckel), analytical Greeks, ACT/365

### Dual-Mode Design
- **Mode 1 (Dhan-match)**: r=0.10, q=0.0, day_count=365 — matches Dhan/NSE displayed values
- **Mode 2 (Theoretical)**: RBI repo rate lookup, NIFTY div yield, day_count=365 — for research/backtesting

## Plan Items

### Phase A: Fix Config (immediate — matches Dhan NOW)

- [x] A1: Fix base.toml parameters to match Dhan
  - Files: config/base.toml
  - Change: `risk_free_rate = 0.10`, `dividend_yield = 0.0`
  - Tests: existing config + calibration tests should show EXACT match
  - Note: Code defaults already correct (0.10, 0.0) — only TOML override is wrong

### Phase B: Precision Foundation (Normal CDF + IV Solver)

- [x] B1: Add `jaeckel` v0.2.0 crate (MIT, zero deps, Cody erfc + Jaeckel IV)
  - Files: Cargo.toml (root workspace)
  - Read Tech Stack Bible first for dependency policy
  - Pin exact version, verify license (MIT), verify no transitive deps
  - Tests: cargo build --workspace

- [x] B2: Replace Normal CDF with Cody's algorithm
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Replace `normal_cdf()` (A&S, 7.5e-8) with Cody's erfc-based: `erfc(-x/sqrt(2))/2`
  - Replace `normal_pdf()` if crate provides it, else keep (PDF is already exact)
  - Use `ln_1p((spot - strike) / strike)` for log-moneyness (ATM precision)
  - Use `f64::mul_add()` for d1/d2 computation (free precision)
  - Tests: test_normal_cdf_precision_16_digits, test_log_moneyness_atm_precision

- [x] B3: Replace IV solver with Jaeckel's "Let's Be Rational"
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Replace `iv_solve()` (Newton-Raphson) with Jaeckel via crate API
  - Keep our `iv_solve()` signature for backward compat, delegate internally
  - Remove `IV_MAX_ITERATIONS`, `IV_INITIAL_GUESS` constants (Jaeckel needs none)
  - Wire config `iv_solver_tolerance` / `iv_solver_max_iterations` to the new solver or remove them
  - Tests: test_iv_roundtrip_machine_epsilon, test_iv_deep_otm_converges, test_iv_near_expiry_converges, test_iv_extreme_vol_converges

### Phase C: Edge Case Guards (QuantLib-grade)

- [x] C1: Add input validation to `greeks_from_iv()` and `compute_greeks_from_iv()`
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Guard: spot <= 0, strike <= 0, iv <= 0, time <= 0 → return sensible defaults
  - Guard: `std_dev < f64::EPSILON` → return intrinsic (QuantLib 3-way branch: ATM/ITM/OTM)
  - Guard: `|d1| > 37.0` → set n(d1)=0 (PDF underflow, skip gamma/vega)
  - Guard: NaN/Infinity check on all outputs before returning
  - Tests: test_guard_zero_spot, test_guard_zero_time_itm, test_guard_zero_time_otm, test_guard_zero_time_atm, test_guard_nan_propagation_blocked

- [x] C2: Fix intrinsic value consistency
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Decision: Use naive intrinsic `max(S-K, 0)` consistently (matches market convention)
  - Both `iv_solve()` intrinsic check AND `greeks_from_iv()` output use same formula
  - Tests: test_intrinsic_consistent_between_solver_and_output

- [x] C3: Fix expiry-day blackout (already uses fractional days)
  - Files: crates/app/src/greeks_pipeline.rs
  - Use `fractional_days` (not integer `days_to_expiry`) for ALL decisions including calibration
  - Change `CalibrationSample.days_to_expiry` from i64 to f64 (fractional)
  - Remove `days_to_expiry <= 0` skip in calibration — use `time_to_expiry <= MIN_TIME` instead
  - Tests: test_expiry_day_greeks_computed, test_calibration_includes_expiry_day

- [x] C4: Add NaN guard before QuestDB persistence
  - Files: crates/app/src/greeks_pipeline.rs
  - Before every `writer.write_*_row()`, check all f64 fields for NaN/Infinity
  - Skip row + WARN log if any NaN detected
  - Tests: test_nan_greeks_not_persisted

### Phase D: Complete Greeks (all 13)

- [x] D1: Add Rho (first-order, rate sensitivity)
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Call: `rho = K * T * exp(-rT) * N(d2)`
  - Put: `rho = -K * T * exp(-rT) * N(-d2)`
  - Add `rho: f64` to `OptionGreeks` struct
  - Tests: test_rho_call_positive, test_rho_put_negative, test_rho_near_expiry_zero

- [x] D2: Add second-order Greeks (Charm, Vanna, Volga, Veta, Speed)
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Charm: delta decay per time (Call/Put differ)
  - Vanna: `-e^(-qT) * n(d1) * d2 / sigma`
  - Volga: `vega * (d1 * d2) / sigma`
  - Veta: vega decay with time
  - Speed: `-(gamma/S) * (1 + d1/(sigma*sqrt(T)))`
  - Add all 5 fields to `OptionGreeks` struct
  - Tests: test_vanna_sign, test_volga_atm, test_charm_direction, test_speed_sign, test_veta_sign

- [x] D3: Add third-order Greeks (Color, Zomma, Ultima)
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Color: gamma decay per time
  - Zomma: `gamma * (d1*d2 - 1) / sigma`
  - Ultima: `(-vega / sigma^2) * [d1*d2*(1-d1*d2) + d1^2 + d2^2]`
  - Add all 3 fields to `OptionGreeks` struct
  - Tests: test_zomma_formula, test_ultima_formula, test_color_formula

- [x] D4: Update QuestDB schema for new Greeks columns
  - Files: crates/storage/src/greeks_persistence.rs
  - Add columns: rho, charm, vanna, volga, veta, speed, color, zomma, ultima
  - Update `write_greeks_row()` to persist all 13
  - Tests: test_greeks_persistence_all_13_columns

### Phase E: Historical Backtesting Support

- [x] E1: Add RBI repo rate lookup table
  - Files: crates/trading/src/greeks/historical_rates.rs (NEW)
  - Hardcode all rate change dates 2020-2026 (14 entries from research)
  - `fn rbi_repo_rate(date: NaiveDate) -> f64` — returns rate in effect on given date
  - Tests: test_rate_lookup_all_dates, test_rate_2020_covid_cut, test_rate_2023_peak

- [x] E2: Add dual-mode rate selection
  - Files: crates/common/src/config.rs, crates/app/src/greeks_pipeline.rs
  - Add `rate_mode` to GreeksConfig: `"dhan"` (fixed 10%) or `"theoretical"` (RBI lookup)
  - Default: `"dhan"` — matches Dhan/NSE displayed values
  - Pipeline selects rate based on mode + date
  - Tests: test_dhan_mode_uses_10_percent, test_theoretical_mode_uses_rbi_lookup

### Phase F: Verification & Tests

- [x] F1: Add Hull textbook reference value tests
  - Files: crates/trading/src/greeks/black_scholes.rs
  - S=100, K=100, T=1, r=5%, q=0%, sigma=20%
  - Assert all 13 Greeks match Hull to 1e-10
  - Tests: test_hull_reference_all_13_greeks

- [x] F2: Add mathematical invariant tests (machine epsilon)
  - Files: crates/trading/src/greeks/black_scholes.rs
  - Put-call parity: `C - P = S*e^(-qT) - K*e^(-rT)` to 1e-14
  - `delta_C - delta_P = e^(-qT)` to 1e-14
  - `gamma_C == gamma_P` to 1e-15
  - `vega_C == vega_P` to 1e-15
  - IV roundtrip: `price → IV → price` to 1e-14
  - Tests: test_parity_machine_epsilon, test_delta_parity, test_iv_roundtrip_1e14

- [x] F3: Add Neumaier summation for portfolio Greeks aggregation
  - Files: crates/trading/src/greeks/mod.rs
  - `fn neumaier_sum(values: &[f64]) -> f64` — compensated summation
  - Tests: test_neumaier_cancellation_resistance

- [x] F4: Build and verify
  - cargo fmt --check
  - cargo clippy --workspace -- -D warnings -W clippy::perf
  - cargo test --workspace
  - cargo build --release --workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Config fix only (A1) | Dhan match improves immediately (r/q were wrong) |
| 2 | Normal CDF upgrade | Put-call parity holds to 1e-14 (was 1e-6) |
| 3 | IV solver upgrade | Deep OTM options converge (was failing) |
| 4 | Expiry day | Greeks computed all day (was skipped entirely) |
| 5 | NaN input | Blocked before QuestDB (was propagating) |
| 6 | All 13 Greeks | Full risk picture for backtesting |
| 7 | Historical backtest 2022 | Uses r=5.90% (Sep 2022 repo rate) |
| 8 | Dhan mode | Exact match with Dhan displayed values |
