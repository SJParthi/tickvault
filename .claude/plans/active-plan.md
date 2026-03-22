# Implementation Plan: Options Greeks Engine (Black-Scholes)

**Status:** IN_PROGRESS
**Date:** 2026-03-22
**Approved by:** Parthiban ("yes dude do it just go ahead")

## Plan Items

- [x] Item 1: Create greeks module with Black-Scholes pricing + IV solver + all Greeks
  - Files: crates/trading/src/greeks/mod.rs (new), crates/trading/src/greeks/black_scholes.rs (new)
  - Contains: normal_cdf(), bs_price(), iv_solve(), delta(), gamma(), theta(), vega(), all O(1)
  - Tests: Known-value tests against published BS tables, edge cases (ATM, deep ITM/OTM, zero time)

- [x] Item 2: Add PCR calculator + Buildup classifier
  - Files: crates/trading/src/greeks/pcr.rs (new), crates/trading/src/greeks/buildup.rs (new)
  - Tests: PCR ratio, all 4 buildup quadrants

- [x] Item 3: Add OptionGreeksSnapshot type to common crate
  - Files: crates/common/src/tick_types.rs (extend)
  - Struct: OptionGreeksSnapshot { iv, delta, gamma, theta, vega, bs_price, intrinsic, extrinsic, pcr }
  - Tests: test_option_greeks_snapshot_default_all_zeros, test_option_greeks_snapshot_is_copy, test_option_greeks_snapshot_typical_atm_call

- [x] Item 4: Add Greeks constants to config
  - Files: config/base.toml, crates/common/src/constants.rs, crates/common/src/config.rs
  - Constants: RISK_FREE_RATE, DIVIDEND_YIELD, IV_SOLVER_MAX_ITERATIONS, IV_SOLVER_TOLERANCE, IV_MIN_BOUND, IV_MAX_BOUND
  - Config struct: GreeksConfig { risk_free_rate, dividend_yield, iv_solver_max_iterations, iv_solver_tolerance }
  - Tests: test_risk_free_rate_reasonable, test_dividend_yield_reasonable, test_iv_solver_max_iterations_positive, test_iv_solver_tolerance_small_positive, test_iv_bounds_valid_range

- [x] Item 5: Register greeks module in trading crate
  - Files: crates/trading/src/lib.rs

## Additional work (not in original plan)

- [x] Wire ensure_greeks_tables() into main.rs boot sequence (both paths)
  - Files: crates/app/src/main.rs
  - Tables: option_greeks, pcr_snapshots, dhan_option_chain_raw, greeks_verification

- [x] Upgrade Loki healthcheck from --version to -health
  - Files: deploy/docker/docker-compose.yml

- [x] Fix compiler warnings (unused imports in option_chain/client.rs, unused vars in valkey_cache.rs)
  - Files: crates/core/src/option_chain/client.rs, crates/storage/src/valkey_cache.rs

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | ATM NIFTY call, 3 days to expiry, IV=30% | Delta ~0.5, Theta large negative |
| 2 | Deep ITM option (delta ~1.0) | IV solver converges, delta near 1.0 |
| 3 | Deep OTM option (delta ~0.0) | IV solver converges, delta near 0.0 |
| 4 | Zero time to expiry | Intrinsic value only, no time value |
| 5 | OI up + Price up | Long Buildup classification |
| 6 | OI down + Price up | Short Covering classification |
