# Implementation Plan: Fix 2 Dedup Key Gaps (candles_1s + derivative_contracts)

**Status:** VERIFIED
**Date:** 2026-03-18
**Approved by:** Parthiban

## Plan Items

- [x] **Item 1: Fix candles_1s dedup key to include segment**
  - `DEDUP_KEY_CANDLES_1S`: `"security_id"` → `"security_id, segment"`
  - Files: `crates/storage/src/materialized_views.rs`
  - Tests: `test_candles_1s_dedup_key_includes_segment`

- [x] **Item 2: Fix derivative_contracts dedup key to include underlying_symbol (I-P1-05)**
  - `DEDUP_KEY_DERIVATIVE_CONTRACTS`: `"security_id"` → `"security_id, underlying_symbol"`
  - Files: `crates/storage/src/instrument_persistence.rs`
  - Tests: `test_dedup_key_derivative_contracts_includes_security_id` (update to verify both fields)

- [x] **Item 3: Add/update tests for both fixes**
  - materialized_views.rs: add `test_candles_1s_dedup_key_includes_segment`
  - instrument_persistence.rs: update existing test to assert `underlying_symbol` present
  - schema_validation.rs: already asserts the right value (validates against local const — now will match real code)
  - Files: `crates/storage/src/materialized_views.rs`, `crates/storage/src/instrument_persistence.rs`

- [x] **Item 4: Build & test — verify all pass**
  - `cargo build --workspace && cargo test --workspace`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | IDX_I and NSE_EQ share security_id for candles_1s | Separate rows (segment in dedup key) |
| 2 | Security_id reused across NIFTY/BANKNIFTY on different days | Separate rows (underlying_symbol in dedup key) |
| 3 | Same security_id + same segment, same timestamp | Dedup merges correctly (upsert) |
