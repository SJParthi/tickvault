# Implementation Plan: Auth + Portfolio + Funds/Margin + Full Market Depth — Full E2E Coverage

**Status:** VERIFIED
**Date:** 2026-03-17
**Approved by:** user (session continuation)

## Summary

Implement 100% API coverage for:
- Authentication (fill remaining gaps)
- Portfolio & Positions (build from ~15% → 100%)
- Funds & Margin (build from 0% → 100%)
- Full Market Depth 20/200-level integration (parsers done, add dispatch + subscription + validation)
- Historical Data (already 100% — verify only)

## Plan Items

### A. Authentication Gaps (fill remaining ~5%)

- [x] A1. Add `UserProfileResponse` struct to `crates/core/src/auth/types.rs`
  - Fields per doc: `dhanClientId`, `tokenValidity` (DD/MM/YYYY HH:MM IST format), `activeSegment`, `ddpi`, `mtf`, `dataPlan`, `dataValidity`
  - Tests: `test_user_profile_response_deserializes`, `test_user_profile_token_validity_format`

- [x] A2. Add `get_user_profile()` method to `crates/core/src/auth/token_manager.rs`
  - Endpoint: `GET /v2/profile` with `access-token` header
  - Returns `UserProfileResponse`

- [x] A3. Add `pre_market_check()` to `crates/core/src/auth/token_manager.rs`
  - At 08:45 IST, verify: dataPlan == "Active", activeSegment contains "Derivative", token > 4h until expiry
  - On failure: CRITICAL alert via Telegram + token rotation attempt

- [x] A4. Add Dhan Static IP API types to `crates/core/src/auth/types.rs`
  - `SetIpRequest`, `SetIpResponse`, `ModifyIpRequest`, `ModifyIpResponse`, `GetIpResponse`
  - Tests: `test_set_ip_request_serializes`, `test_get_ip_response_deserializes`, `test_modify_ip_request_serializes_camel_case`, `test_set_ip_response_deserializes`, `test_modify_ip_response_deserializes`

- [x] A5. Add Dhan IP API methods to `crates/core/src/network/ip_verifier.rs`
  - `set_ip()`, `modify_ip()`, `get_ip()` public async functions
  - Tests: `test_set_ip_request_format`, `test_get_ip_response_parsing`

- [x] A6. Add endpoint path constants to `crates/common/src/constants.rs`
  - 12 endpoint paths + 2 depth WS URLs + fix FEED_UNSUBSCRIBE_TWENTY_DEPTH 24→25

### B. Portfolio & Positions (build from ~15% → 100%)

- [x] B1. Add `DhanHoldingResponse` struct to `crates/trading/src/oms/types.rs`
  - 14 fields with special snake_case renames for `mtf_tq_qty` and `mtf_qty`
  - Tests: `test_holding_response_deserializes`, `test_holding_response_mtf_snake_case_fields`

- [x] B2. Complete `DhanPositionResponse` — add all missing fields
  - Added: rbiReferenceRate, carryForward*, day*, crossCurrency
  - Tests: `test_position_response_all_fields_deserialize`, `test_position_response_carry_forward_fields`

- [x] B3. Add `DhanConvertPositionRequest` struct — convertQty as STRING
  - Tests: `test_convert_position_request_serializes`, `test_convert_position_convert_qty_is_string`

- [x] B4. Add `DhanExitAllResponse` struct
  - Tests: `test_exit_all_response_deserializes`

- [x] B5. Add `get_holdings()` method to api_client.rs
  - Tests: `test_get_holdings_success`, `test_get_holdings_empty`, `test_get_holdings_api_error_401`

- [x] B6. Add `convert_position()` method to api_client.rs
  - Tests: `test_convert_position_success_202`, `test_convert_position_rate_limited_429`

- [x] B7. Add `exit_all_positions()` method to api_client.rs
  - Tests: `test_exit_all_positions_success`

### C. Funds & Margin (build from 0% → 100%)

- [x] C1. Add `MarginCalculatorRequest` struct — securityId as STRING
  - Tests: `test_margin_calculator_request_serializes_camel_case`, `test_margin_calculator_security_id_is_string`

- [x] C2. Add `MarginCalculatorResponse` struct — leverage as STRING
  - Tests: `test_margin_calculator_response_deserializes`, `test_margin_calculator_leverage_is_string`

- [x] C3. Add `MultiMarginRequest`, `MarginScript`, `MultiMarginResponse` — ALL STRING values
  - Tests: `test_multi_margin_request_serializes`, `test_multi_margin_response_all_strings`

- [x] C4. Add `FundLimitResponse` — preserves `availabelBalance` typo
  - Tests: `test_fund_limit_response_deserializes`, `test_fund_limit_availabel_balance_typo`

- [x] C5. Add `calculate_margin()` method to api_client.rs
  - Tests: `test_calculate_margin_success`, `test_calculate_margin_api_error_400`

- [x] C6. Add `calculate_multi_margin()` method to api_client.rs
  - Tests: `test_calculate_multi_margin_success`

- [x] C7. Add `get_fund_limit()` method to api_client.rs
  - Tests: `test_get_fund_limit_success`, `test_get_fund_limit_rate_limited`

### D. Historical Data — Verified Complete

- [x] D1. Verify historical data implementation is 100% complete
  - All 7 items verified: 2 endpoints, columnar parsing, string intervals, 90-day max, non-inclusive toDate, correct field types, epoch timestamps

### E. Full Market Depth Integration

- [x] E1. Fix `FEED_UNSUBSCRIBE_TWENTY_DEPTH` from 24 → 25
  - Per annexure-enums.md: "UnsubscribeFullDepth is 25, NOT 24"

- [x] E2. Add `ParsedFrame::DeepDepth` variant to parser/types.rs
  - Tests: `test_parsed_frame_deep_depth_variant`

- [x] E3. Add `dispatch_deep_depth_frame()` to parser/dispatcher.rs
  - Tests: `test_dispatch_deep_depth_bid`, `test_dispatch_deep_depth_ask`, `test_dispatch_deep_depth_too_short`

- [x] E4. Add `split_stacked_depth_packets()` to parser/dispatcher.rs
  - Tests: `test_split_stacked_single_packet`, `test_split_stacked_bid_ask_pair`, `test_split_stacked_multiple_instruments`, `test_split_stacked_empty`, `test_split_stacked_trailing_bytes_ignored`

- [x] E5. Add `TwoHundredDepthSubscriptionRequest` to websocket/types.rs
  - Tests: `test_two_hundred_depth_subscription_serialize`, `test_two_hundred_depth_subscription_deserialize_roundtrip`, `test_two_hundred_depth_unsubscribe_request_code_25`, `test_two_hundred_depth_security_id_is_string`

- [x] E6. Add 200-level subscription/unsubscription builders
  - Tests: `test_two_hundred_depth_subscription_nse_eq`, `test_two_hundred_depth_subscription_nse_fno`, `test_two_hundred_depth_unsubscription_request_code_25`

- [x] E7. Add NSE-only validation for depth subscriptions
  - Tests: `test_validate_depth_segment_nse_eq_ok`, `test_validate_depth_segment_nse_fno_ok`, `test_validate_depth_segment_bse_eq_rejected`, `test_validate_depth_segment_bse_fno_rejected`, `test_validate_depth_segment_idx_rejected`, `test_two_hundred_depth_subscription_rejects_bse`, `test_two_hundred_depth_subscription_rejects_idx`, `test_two_hundred_depth_unsubscription_rejects_non_nse`

## Build Verification

- fmt: PASS
- clippy: PASS (zero warnings)
- test: PASS (2,652 tests, 0 failures)

## Files Modified

| File | Changes |
|------|---------|
| `crates/common/src/constants.rs` | 12 endpoint constants, 2 depth WS URLs, fix FEED_UNSUBSCRIBE 24→25 |
| `crates/core/src/auth/types.rs` | UserProfileResponse, IP API types (5 structs), 7 tests |
| `crates/core/src/auth/token_manager.rs` | get_user_profile(), pre_market_check() |
| `crates/core/src/network/ip_verifier.rs` | set_ip(), modify_ip(), get_ip(), 2 tests |
| `crates/trading/src/oms/types.rs` | Holdings, ConvertPosition, ExitAll, Margin, Fund types (9 structs), 16 tests |
| `crates/trading/src/oms/api_client.rs` | 6 new API methods, 11 async tests |
| `crates/core/src/parser/types.rs` | ParsedFrame::DeepDepth variant, 1 test |
| `crates/core/src/parser/dispatcher.rs` | dispatch_deep_depth_frame(), split_stacked_depth_packets(), 8 tests |
| `crates/core/src/websocket/types.rs` | TwoHundredDepthSubscriptionRequest, 4 tests |
| `crates/core/src/websocket/subscription_builder.rs` | 200-level builders, NSE-only validation, 10 tests |
| `crates/core/src/pipeline/tick_processor.rs` | DeepDepth match arm in frame dispatch |
