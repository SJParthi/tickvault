# Implementation Plan: Options Greeks Engine (Black-Scholes)

**Status:** VERIFIED
**Date:** 2026-03-22
**Approved by:** Parthiban ("yes dude do it just go ahead")

## Plan Items

- [x] Item 1: Create greeks module with Black-Scholes pricing + IV solver + all Greeks
  - Files: crates/trading/src/greeks/mod.rs, crates/trading/src/greeks/black_scholes.rs
  - Tests: test_bs_call_price_atm, test_iv_solve_call_roundtrip, test_compute_greeks_atm_call

- [x] Item 2: Add PCR calculator + Buildup classifier
  - Files: crates/trading/src/greeks/pcr.rs, crates/trading/src/greeks/buildup.rs
  - Tests: test_pcr_basic, test_long_buildup, test_short_covering

- [x] Item 3: Add OptionGreeksSnapshot type to common crate
  - Files: crates/common/src/tick_types.rs
  - Tests: test_option_greeks_snapshot_default_all_zeros, test_option_greeks_snapshot_is_copy, test_option_greeks_snapshot_typical_atm_call

- [x] Item 4: Add Greeks constants to config
  - Files: config/base.toml, crates/common/src/constants.rs, crates/common/src/config.rs
  - Tests: test_risk_free_rate_reasonable, test_dividend_yield_reasonable, test_iv_bounds_valid_range

- [x] Item 5: Register greeks module in trading crate
  - Files: crates/trading/src/lib.rs
  - Tests: test_compute_greeks_atm_call

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | ATM NIFTY call, 3 days to expiry, IV=30% | Delta ~0.5, Theta large negative |
| 2 | Deep ITM option (delta ~1.0) | IV solver converges, delta near 1.0 |
| 3 | Deep OTM option (delta ~0.0) | IV solver converges, delta near 0.0 |
| 4 | Zero time to expiry | Intrinsic value only, no time value |
| 5 | OI up + Price up | Long Buildup classification |
| 6 | OI down + Price up | Short Covering classification |
