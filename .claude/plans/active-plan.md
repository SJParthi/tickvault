# Implementation Plan: Gap Enforcement Completion & Final Verification

**Status:** DRAFT
**Date:** 2026-04-04
**Approved by:** pending

## Context

Deep research via 8 parallel agents covering: Dhan Python SDK comparison, all Dhan doc URLs,
QuestDB crash resilience, monitoring system, gap enforcement, sandbox enforcement, and full
workspace test suite verification.

### Research Results Summary

**Verified CORRECT (no changes needed):**
1. **All binary parser offsets** match Python SDK `struct.unpack` format strings exactly
   - Market Feed: `<BHBIfI>` (ticker 16B), `<BHBIfHIfIIIffff>` (quote 50B), `<BHBIfHIfIIIIIIffff100s>` (full 162B)
   - Full Depth: `<hBBiI>` (12-byte header), `<dII>` (f64 price + u32 qty + u32 orders per level)
   - Order Update: JSON `wss://api-order-update.dhan.co`, MsgCode=42, PascalCase fields
2. **QuestDB crash resilience** — 3-layer zero-tick-loss: ring buffer (300K) → disk spill → startup recovery
3. **Sandbox/dry-run** — `dry_run=true` default, OMS blocks all HTTP when enabled, paper order IDs
4. **Monitoring** — 30+ Telegram events, Prometheus metrics, circuit breaker, auto-reconnect
5. **All 1,317 tests pass**, 0 failures across 17 test binaries
6. **Compile-time assertions** enforce all byte offsets mechanically in constants.rs

### Gaps Reassessed (from original 6 "untested" → only 2 real gaps)

**Actually FULLY TESTED (reclassified after deeper investigation):**
- I-P1-02: Delta detector has tests for ALL fields (lot_size, tick_size, strike_price, option_type, exchange_segment, display_name, symbol_name)
- I-P1-03: Security ID reuse detection has `test_p1_03_security_id_reuse_across_underlyings`
- I-P1-05: DEDUP_KEY includes `underlying_symbol` — 6+ tests across storage and common crates
- AUTH-GAP-01: TokenState has `is_valid()`, `needs_refresh()`, `time_until_refresh()` — 15+ tests

**Real remaining gaps (2):**
- I-P0-02: Only checks derivative count > 0, NOT minimum threshold (100)
- I-P0-03: ManagedOrder has no `expiry_date` field for OMS Gate 4 expiry validation

**Partially tested (1):**
- I-P0-06: Emergency download implementation + basic tests exist, but test doesn't verify CRITICAL log level

### Additional Improvements Identified

- Gap enforcement test file comments need updating to reflect actual coverage
- I-P0-02 minimum threshold should be a configurable constant

## Plan Items

- [ ] Fix I-P0-02: Add minimum derivative count threshold validation (configurable constant, currently only checks > 0)
  - Files: crates/core/src/instrument/universe_builder.rs, crates/common/src/constants.rs
  - Tests: test_validation_derivative_count_below_minimum_fails, test_validation_derivative_count_at_minimum_passes

- [ ] Fix I-P0-03: Add expiry_date awareness to OMS order validation path
  - Files: crates/trading/src/oms/engine.rs, crates/common/src/order_types.rs
  - Tests: test_expired_contract_rejected_at_gate_4, test_valid_contract_passes_gate_4

- [ ] Fix I-P0-06: Add CRITICAL log level assertion to emergency download test
  - Files: crates/core/tests/gap_enforcement.rs
  - Tests: test_i_p0_06_emergency_download_logs_critical

- [ ] Update gap enforcement test documentation comments to reflect actual coverage status
  - Files: crates/trading/tests/gap_enforcement.rs (I-P0-03 comment), crates/core/tests/gap_enforcement.rs (I-P0-02, I-P0-06 comments)
  - Tests: existing tests, no new tests needed

- [ ] Run full build verification (fmt + clippy + test) to confirm zero regressions
  - Files: none (verification only)
  - Tests: all workspace tests

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Universe has 50 derivatives (below 100 threshold) | Hard error, not silent continue |
| 2 | Universe has 100+ derivatives | Passes validation |
| 3 | Order submitted for expired contract | Rejected with CRITICAL log before reaching Dhan API |
| 4 | Order submitted for valid contract | Passes through OMS normally |
| 5 | Emergency download during market hours with no cache | CRITICAL log emitted (not just INFO) |
| 6 | QuestDB crashes at 9:20 AM, reconnects at 10:15 AM | Zero ticks lost — ring buffer + disk spill drain |
| 7 | Full test suite after all changes | 1,317+ tests pass, 0 failures |
