# Implementation Plan: Complete Audit Remediation

**Status:** APPROVED
**Date:** 2026-03-19
**Approved by:** Parthiban (explicit instruction: "Cover everything here itself now")

## Plan Items

- [ ] 1. Fix benchmark budget names + create 3 missing benchmarks
  - Files: quality/benchmark-budgets.toml, crates/trading/benches/oms.rs, crates/common/benches/calendar.rs, crates/common/benches/config.rs
  - Tests: bench_oms_state_transition, bench_market_hour_validation, bench_config_toml_load

- [ ] 2. Add tests for 3 zero-coverage modules
  - Files: crates/trading/src/strategy/hot_reload.rs, crates/storage/src/calendar_persistence.rs, crates/storage/src/valkey_cache.rs
  - Tests: test_hot_reloader_*, test_calendar_*, test_valkey_*

- [ ] 3. Implement 5 missing gap enforcement tests
  - Files: crates/core/tests/gap_enforcement.rs, crates/core/src/instrument/universe_builder.rs
  - Tests: test_i_p0_01 through test_i_p2_02

- [ ] 4. Add lint enforcement to api/app crates
  - Files: crates/api/src/lib.rs, crates/app/src/main.rs

- [ ] 5. Fix CORS + auth fail-closed
  - Files: crates/api/src/lib.rs, crates/api/src/middleware.rs, crates/common/src/config.rs

- [ ] 6. Add proptest to common crate
  - Files: crates/common/Cargo.toml, crates/common/tests/proptest_common.rs

- [ ] 7. Add panic safety tests to api crate
  - Files: crates/api/tests/panic_safety.rs

- [ ] 8. Add DHAT tests for deep depth parsing
  - Files: crates/core/tests/dhat_deep_depth.rs

- [ ] 9. Add Display/Debug tests for trading crate
  - Files: crates/trading/src/oms/state_machine.rs, crates/trading/src/oms/circuit_breaker.rs

- [ ] 10. Wire or delete 3 orphaned modules
  - Files: crates/common/src/lib.rs, crates/core/src/lib.rs, crates/core/src/instrument/mod.rs

- [ ] 11. Extract IST offset helper to common
  - Files: crates/common/src/trading_calendar.rs, 13+ consumer files

- [ ] 12. Consolidate segment_code_to_str
  - Files: crates/storage/src/tick_persistence.rs, crates/storage/src/candle_persistence.rs

- [ ] 13. Add OMS end-to-end integration test
  - Files: crates/trading/tests/oms_integration.rs

- [ ] 14. Fix CI cosmetic issues
  - Files: .github/workflows/ci.yml
