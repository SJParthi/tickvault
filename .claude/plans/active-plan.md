# Implementation Plan: Auth + Portfolio + Funds/Margin + Full Market Depth — Full E2E Coverage

**Status:** DRAFT
**Date:** 2026-03-17
**Approved by:** pending

## Summary

Implement 100% API coverage for:
- Authentication (fill remaining gaps)
- Portfolio & Positions (build from ~15% → 100%)
- Funds & Margin (build from 0% → 100%)
- Full Market Depth 20/200-level integration (parsers done, add dispatch + subscription + validation)
- Historical Data (already 100% — verify only)

## Plan Items

### A. Authentication Gaps (fill remaining ~5%)

- [ ] A1. Add `UserProfileResponse` struct to `crates/core/src/auth/types.rs`
  - Fields per doc: `dhanClientId`, `tokenValidity` (DD/MM/YYYY HH:MM IST format), `activeSegment`, `ddpi`, `mtf`, `dataPlan`, `dataValidity`
  - Tests: `test_user_profile_response_deserializes`, `test_user_profile_token_validity_format`

- [ ] A2. Add `get_user_profile()` method to `crates/core/src/auth/token_manager.rs`
  - Endpoint: `GET /v2/profile` with `access-token` header
  - Returns `UserProfileResponse`
  - Tests: `test_get_user_profile_success`, `test_get_user_profile_auth_error`

- [ ] A3. Add `pre_market_check()` to `crates/core/src/auth/token_manager.rs`
  - At 08:45 IST, verify: dataPlan == "Active", activeSegment contains "Derivative", token > 4h until expiry
  - On failure: CRITICAL alert via Telegram + token rotation attempt
  - Tests: `test_pre_market_check_passes_valid`, `test_pre_market_check_fails_no_derivative`, `test_pre_market_check_fails_inactive_data_plan`, `test_pre_market_check_fails_token_near_expiry`

- [ ] A4. Add Dhan Static IP API types to `crates/core/src/auth/types.rs`
  - `SetIpRequest` (dhanClientId, ip, ipFlag), `SetIpResponse`
  - `ModifyIpRequest` (dhanClientId, ip, ipFlag), `ModifyIpResponse`
  - `GetIpResponse` (ip, ipFlag, modifyDatePrimary, modifyDateSecondary)
  - Tests: `test_set_ip_request_serializes`, `test_get_ip_response_deserializes`

- [ ] A5. Add Dhan IP API methods to `crates/core/src/network/ip_verifier.rs`
  - `set_ip()` → `POST /v2/ip/setIP`
  - `modify_ip()` → `PUT /v2/ip/modifyIP` (7-day cooldown warning)
  - `get_ip()` → `GET /v2/ip/getIP`
  - Tests: `test_set_ip_request_format`, `test_get_ip_response_parsing`

- [ ] A6. Add endpoint path constants to `crates/common/src/constants.rs`
  - Auth: `DHAN_USER_PROFILE_PATH`, `DHAN_SET_IP_PATH`, `DHAN_MODIFY_IP_PATH`, `DHAN_GET_IP_PATH`
  - Portfolio: `DHAN_HOLDINGS_PATH`, `DHAN_POSITIONS_PATH`, `DHAN_POSITIONS_CONVERT_PATH`
  - Margin: `DHAN_MARGIN_CALCULATOR_PATH`, `DHAN_MARGIN_CALCULATOR_MULTI_PATH`, `DHAN_FUND_LIMIT_PATH`
  - Depth WS: `DHAN_TWENTY_DEPTH_WS_BASE_URL`, `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL`
  - Tests: `test_dhan_endpoint_path_constants`, `test_depth_ws_url_constants`

### B. Portfolio & Positions (build from ~15% → 100%)

- [ ] B1. Add `DhanHoldingResponse` struct to `crates/trading/src/oms/types.rs`
  - Fields per doc: exchange, tradingSymbol, securityId, isin, totalQty, dpQty, t1Qty, mtf_tq_qty (snake_case!), mtf_qty (snake_case!), availableQty, collateralQty, avgCostPrice, lastTradedPrice
  - Note: `mtf_tq_qty` and `mtf_qty` use snake_case in Dhan API (inconsistent)
  - Tests: `test_holding_response_deserializes`, `test_holding_response_mtf_snake_case_fields`

- [ ] B2. Complete `DhanPositionResponse` — add all missing fields
  - Add: `rbiReferenceRate`, `carryForwardBuyQty`, `carryForwardSellQty`, `carryForwardBuyValue`, `carryForwardSellValue`, `dayBuyQty`, `daySellQty`, `dayBuyValue`, `daySellValue`, `crossCurrency`
  - Tests: `test_position_response_all_fields_deserialize`, `test_position_response_carry_forward_fields`

- [ ] B3. Add `DhanConvertPositionRequest` struct to `crates/trading/src/oms/types.rs`
  - Fields: dhanClientId, fromProductType, exchangeSegment, positionType, securityId (STRING), tradingSymbol, convertQty (STRING!), toProductType
  - CRITICAL: convertQty is STRING not integer
  - Tests: `test_convert_position_request_serializes`, `test_convert_position_convert_qty_is_string`

- [ ] B4. Add `DhanExitAllResponse` struct to `crates/trading/src/oms/types.rs`
  - Fields: status, message
  - Tests: `test_exit_all_response_deserializes`

- [ ] B5. Add `get_holdings()` method to `crates/trading/src/oms/api_client.rs`
  - Endpoint: `GET /v2/holdings` with access-token header
  - Returns `Vec<DhanHoldingResponse>`
  - Tests: `test_get_holdings_success`, `test_get_holdings_empty`, `test_get_holdings_rate_limited`

- [ ] B6. Add `convert_position()` method to `crates/trading/src/oms/api_client.rs`
  - Endpoint: `POST /v2/positions/convert` with access-token header
  - Expects 202 Accepted
  - Tests: `test_convert_position_success_202`, `test_convert_position_error`

- [ ] B7. Add `exit_all_positions()` method to `crates/trading/src/oms/api_client.rs`
  - Endpoint: `DELETE /v2/positions` with access-token header
  - DANGER: Exits ALL positions AND cancels ALL pending orders
  - Returns `DhanExitAllResponse`
  - Tests: `test_exit_all_positions_success`, `test_exit_all_positions_error`

### C. Funds & Margin (build from 0% → 100%)

- [ ] C1. Add `MarginCalculatorRequest` struct to `crates/trading/src/oms/types.rs`
  - Fields: dhanClientId, exchangeSegment, transactionType, quantity (i64), productType, securityId (STRING), price (f64), triggerPrice (f64)
  - Uses `#[serde(rename_all = "camelCase")]`
  - Tests: `test_margin_calculator_request_serializes_camel_case`, `test_margin_calculator_security_id_is_string`

- [ ] C2. Add `MarginCalculatorResponse` struct to `crates/trading/src/oms/types.rs`
  - Fields: totalMargin, spanMargin, exposureMargin, availableBalance (NOTE: correct spelling here), variableMargin, insufficientBalance, brokerage (all f64), leverage (STRING!)
  - Tests: `test_margin_calculator_response_deserializes`, `test_margin_calculator_leverage_is_string`

- [ ] C3. Add `MultiMarginRequest`, `MarginScript`, `MultiMarginResponse` to `crates/trading/src/oms/types.rs`
  - Request: camelCase (includePosition, includeOrders, scripts array)
  - Response: snake_case, ALL values are STRINGS (not floats!)
  - Fields: total_margin, span_margin, exposure_margin, equity_margin, fo_margin, commodity_margin, currency, hedge_benefit
  - Tests: `test_multi_margin_request_serializes`, `test_multi_margin_response_all_strings`

- [ ] C4. Add `FundLimitResponse` struct to `crates/trading/src/oms/types.rs`
  - CRITICAL: `availabelBalance` is a TYPO in Dhan API — use `#[serde(rename = "availabelBalance")]`
  - Fields: dhanClientId, availabelBalance, sodLimit, collateralAmount, receiveableAmount, utilizedAmount, blockedPayoutAmount, withdrawableBalance
  - Tests: `test_fund_limit_response_deserializes`, `test_fund_limit_availabel_balance_typo`

- [ ] C5. Add `calculate_margin()` method to `crates/trading/src/oms/api_client.rs`
  - Endpoint: `POST /v2/margincalculator` with access-token + client-id headers
  - Tests: `test_calculate_margin_success`, `test_calculate_margin_insufficient_balance`

- [ ] C6. Add `calculate_multi_margin()` method to `crates/trading/src/oms/api_client.rs`
  - Endpoint: `POST /v2/margincalculator/multi` with access-token + client-id headers
  - Tests: `test_calculate_multi_margin_success`

- [ ] C7. Add `get_fund_limit()` method to `crates/trading/src/oms/api_client.rs`
  - Endpoint: `GET /v2/fundlimit` with access-token header
  - Tests: `test_get_fund_limit_success`, `test_get_fund_limit_error`

### D. Historical Data — Verified Complete

- [ ] D1. Verify historical data implementation is 100% complete
  - Candle fetcher: multi-timeframe (1m, 5m, 15m, 60m, daily) ✓
  - Cross-verification: QuestDB integrity checks ✓
  - Persistence: CandlePersistenceWriter + LiveCandleWriter ✓
  - Candle aggregator: 1-second base candles from ticks ✓
  - Tests: 50+ existing tests ✓
  - No code changes needed — just verification

### E. Full Market Depth Integration (parsers done → add dispatch + subscription + validation)

**Status: Parsers 100% done. Integration ~40% missing.**

- [ ] E1. Fix `FEED_UNSUBSCRIBE_TWENTY_DEPTH` from 24 → 25 in `crates/common/src/constants.rs`
  - Per annexure-enums.md: "UnsubscribeFullDepth is 25, NOT 24"
  - RequestCode 23 = subscribe (both 20 & 200 level), 25 = unsubscribe (both)
  - Tests: `test_depth_unsubscribe_code_is_25`

- [ ] E2. Add `ParsedFrame::DeepDepth` variant to `crates/core/src/parser/types.rs`
  - Carries: security_id, exchange_segment_code, side (Bid/Ask), levels (Vec<DeepDepthLevel>), received_at_nanos
  - Re-export `DepthSide` from deep_depth module
  - Tests: `test_parsed_frame_deep_depth_variant`

- [ ] E3. Add `dispatch_deep_depth_frame()` to `crates/core/src/parser/dispatcher.rs`
  - Separate entry point for depth WS connections (12-byte header, not 8-byte)
  - Routes feed codes 41 (Bid) and 51 (Ask) via existing `parse_twenty_depth_packet` / `parse_two_hundred_depth_packet`
  - Tests: `test_dispatch_deep_depth_bid`, `test_dispatch_deep_depth_ask`, `test_dispatch_deep_depth_unknown_code`

- [ ] E4. Add `split_stacked_depth_packets()` to `crates/core/src/parser/dispatcher.rs`
  - For 20-level: multiple instrument bid/ask pairs stacked in one WS message
  - Splits by reading message_length from bytes 0-1 of each sub-packet
  - Tests: `test_split_stacked_single_packet`, `test_split_stacked_two_instruments`, `test_split_stacked_empty`

- [ ] E5. Add 200-level subscription types to `crates/core/src/websocket/types.rs`
  - `TwoHundredDepthSubscriptionRequest` — flat JSON: `{ "RequestCode": 23, "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" }`
  - No InstrumentList array (different from 20-level)
  - Tests: `test_two_hundred_depth_subscription_serializes_flat_json`, `test_two_hundred_depth_no_instrument_list`

- [ ] E6. Add 200-level subscription builder to `crates/core/src/websocket/subscription_builder.rs`
  - `build_two_hundred_depth_subscription_message()` — single instrument, flat JSON
  - `build_two_hundred_depth_unsubscription_message()` — RequestCode 25
  - Tests: `test_two_hundred_depth_subscribe_flat_json`, `test_two_hundred_depth_unsubscribe_code_25`

- [ ] E7. Add NSE-only validation for depth subscriptions to `crates/core/src/websocket/subscription_builder.rs`
  - Reject BSE, MCX, Currency segments for depth subscriptions
  - Only NSE_EQ and NSE_FNO valid for Full Market Depth
  - Tests: `test_depth_nse_only_rejects_bse`, `test_depth_nse_only_accepts_nse_eq`, `test_depth_nse_only_accepts_nse_fno`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | GET /v2/profile returns valid profile | `UserProfileResponse` deserializes, dataPlan/activeSegment parsed |
| 2 | Pre-market check with expired dataPlan | CRITICAL alert, rotation attempt |
| 3 | GET /v2/holdings returns mixed camelCase/snake_case | `mtf_tq_qty` and `mtf_qty` correctly parsed via rename |
| 4 | POST /v2/positions/convert with convertQty as string | Serializes as `"40"` not `40` |
| 5 | DELETE /v2/positions emergency exit | All positions closed + all orders cancelled |
| 6 | POST /v2/margincalculator returns leverage as string | `leverage: "4.00"` deserializes to String |
| 7 | POST /v2/margincalculator/multi returns snake_case strings | All fields parsed as String, not float |
| 8 | GET /v2/fundlimit returns `availabelBalance` typo | Correctly deserialized via `#[serde(rename)]` |
| 9 | GET /v2/ip/getIP returns modification dates | `modifyDatePrimary` parsed |
| 10 | Historical candle system verified complete | No gaps, all endpoints, all timeframes |
| 11 | 20-level depth bid/ask stacked packets | Split correctly, each side parsed independently |
| 12 | 200-level depth single instrument flat JSON | No InstrumentList array, flat fields |
| 13 | BSE instrument depth subscription attempt | Rejected at build time with clear error |
| 14 | 20-level depth unsubscribe | Uses RequestCode 25 (not 24) |

## Files Modified

| File | Changes |
|------|---------|
| `crates/common/src/constants.rs` | Add 12 endpoint path constants, fix FEED_UNSUBSCRIBE_TWENTY_DEPTH 24→25, add depth WS URLs |
| `crates/core/src/auth/types.rs` | Add UserProfileResponse, IP API types |
| `crates/core/src/auth/token_manager.rs` | Add get_user_profile(), pre_market_check() |
| `crates/core/src/network/ip_verifier.rs` | Add set_ip(), modify_ip(), get_ip() |
| `crates/trading/src/oms/types.rs` | Add Holding, ConvertPosition, Margin, Fund types |
| `crates/trading/src/oms/api_client.rs` | Add 6 new API methods + tests |
| `crates/core/src/parser/types.rs` | Add ParsedFrame::DeepDepth variant |
| `crates/core/src/parser/dispatcher.rs` | Add dispatch_deep_depth_frame(), split_stacked_depth_packets() |
| `crates/core/src/websocket/types.rs` | Add TwoHundredDepthSubscriptionRequest |
| `crates/core/src/websocket/subscription_builder.rs` | Add 200-level builders, NSE-only validation |

## Total New Tests: ~65+
