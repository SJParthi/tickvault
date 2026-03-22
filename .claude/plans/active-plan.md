# Implementation Plan: Options Greeks Engine (Black-Scholes)

**Status:** APPROVED
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

- [ ] Item 3: Add OptionGreeks type to common crate
  - Files: crates/common/src/tick_types.rs (extend)
  - Struct: OptionGreeks { iv, delta, gamma, theta, vega, intrinsic, extrinsic, pcr }

- [ ] Item 4: Add Greeks constants to config
  - Files: config/base.toml, crates/common/src/constants.rs
  - Constants: RISK_FREE_RATE, DIVIDEND_YIELD, IV_SOLVER_MAX_ITERATIONS, IV_SOLVER_TOLERANCE

- [x] Item 5: Register greeks module in trading crate
  - Files: crates/trading/src/lib.rs

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | ATM NIFTY call, 3 days to expiry, IV=30% | Delta ~0.5, Theta large negative |
| 2 | Deep ITM option (delta ~1.0) | IV solver converges, delta near 1.0 |
| 3 | Deep OTM option (delta ~0.0) | IV solver converges, delta near 0.0 |
| 4 | Zero time to expiry | Intrinsic value only, no time value |
| 5 | OI up + Price up | Long Buildup classification |
| 6 | OI down + Price up | Short Covering classification |
