# Implementation Plan: Test Coverage Gap Closure

**Status:** VERIFIED
**Date:** 2026-03-16
**Approved by:** Parthiban

## Plan Items

- [x] **1. OMS API Client — HTTP error path tests**
  - Files: `crates/trading/src/oms/api_client.rs`
  - Tests: `test_place_order_success_response`, `test_place_order_rate_limited_429`, `test_place_order_dhan_api_error_non_2xx`, `test_place_order_json_parse_error`, `test_modify_order_success`, `test_modify_order_rate_limited`, `test_cancel_order_success`, `test_cancel_order_api_error`, `test_get_order_success`, `test_get_all_orders_success`, `test_get_all_orders_empty_array`, `test_get_positions_success`, `test_get_positions_empty`, `test_auth_headers_correct`

- [x] **2. Token Manager — lifecycle unit tests**
  - Files: `crates/core/src/auth/token_manager.rs`
  - Tests: `test_token_handle_starts_none`, `test_token_handle_load_after_store`, `test_build_generate_token_url_format`, `test_build_renew_token_url_format`, `test_classify_generate_response_success`, `test_classify_generate_response_missing_token`, `test_token_renewal_needed_check`, `test_token_not_expired_within_validity`

- [x] **3. Historical Candle Fetcher — edge case tests**
  - Files: `crates/core/src/historical/candle_fetcher.rs`
  - Tests: `test_max_response_body_size_is_10mb`, `test_intraday_request_oi_field_serialized`, `test_intraday_request_all_fields_camel_case`, `test_candle_fetch_summary_all_zeroes`, `test_nan_price_detection`, `test_infinity_price_detection`, `test_neg_infinity_price_detection`, `test_normal_price_is_finite`, `test_dhan_response_consistency_check`

- [x] **4. Risk Engine — P&L edge case tests**
  - Files: `crates/trading/src/risk/engine.rs`
  - Tests: `test_position_reversal_long_to_short_pnl`, `test_position_reversal_short_to_long_pnl`, `test_weighted_avg_entry_many_fills`, `test_close_reopen_same_instrument`, `test_short_loss_calculation`, `test_multiple_instruments_pnl_independent`, `test_reset_daily_then_new_fills`, `test_exact_max_lots_boundary`, `test_negative_lots_sell_short`, `test_fill_at_zero_price`

- [x] **5. WebSocket Connection Pool — already comprehensive (32+ tests)**
  - Files: `crates/core/src/websocket/connection_pool.rs`
  - Pre-existing: `test_pool_debug_format`, `test_pool_capacity_exceeded`, `test_pool_empty_instruments_ok`, `test_pool_single_instrument`, `test_pool_exact_capacity_boundary`, `test_pool_round_robin_distribution`, plus 26+ more including proptest

- [x] **6. Notification Service — helper function tests**
  - Files: `crates/core/src/notification/service.rs`
  - Tests: `test_strip_html_tags_unclosed_tag`, `test_strip_html_tags_only_tags`, `test_strip_html_tags_special_chars_in_content`, `test_strip_html_tags_angle_brackets_in_text`, `test_strip_html_tags_multiple_nested_tags`, `test_mask_phone_exact_boundary_8_chars`, `test_mask_phone_9_chars`, `test_mask_phone_empty`, `test_mask_phone_single_char`, `test_disabled_service_all_critical_events_safe`, `test_severity_low_does_not_trigger_sms_path`

- [x] **7. Config Validation — cross-field and boundary tests**
  - Files: `crates/common/src/config.rs`
  - Tests: `test_market_close_before_open_time_values`, `test_order_cutoff_before_close`, `test_historical_lookback_zero_fails`, `test_historical_lookback_over_90_fails`, `test_historical_lookback_at_90_passes`, `test_historical_lookback_at_1_passes`, `test_historical_request_timeout_zero_fails`, `test_historical_disabled_skips_validation`, `test_websocket_excessive_read_timeout_fails`, `test_feed_mode_ticker_passes`, `test_feed_mode_quote_passes`, `test_feed_mode_full_passes`, `test_feed_mode_invalid_string_fails`, `test_feed_mode_case_sensitive`

- [x] **8. Order Update WebSocket — field mapping tests**
  - Files: `crates/core/src/parser/order_update.rs`
  - Tests: `test_product_code_c_is_cnc`, `test_product_code_i_is_intraday`, `test_txn_type_b_is_buy`, `test_txn_type_s_is_sell`, `test_source_p_is_api`, `test_opt_type_xx_is_non_option`, `test_opt_type_ce_is_call`, `test_opt_type_pe_is_put`, `test_leg_number_mapping`, `test_super_order_remark`, `test_correlation_id_roundtrip`, `test_extra_unknown_fields_ignored`, `test_partial_fill_fields`

- [x] **9. Build verification**
  - `cargo test --workspace` — 2380 tests pass, 0 failures
  - `cargo clippy --workspace --tests` — 0 warnings

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | OMS client receives HTTP 429 | Returns `OmsError::DhanRateLimited` |
| 2 | OMS client receives HTTP 500 with body | Returns `OmsError::DhanApiError` with body |
| 3 | OMS client receives valid JSON | Deserializes into typed response |
| 4 | Risk engine: long 10 → sell 15 → short 5 | Correct realized P&L, entry price = fill price |
| 5 | Risk engine: many partial fills | Weighted avg doesn't drift from true value |
| 6 | Config: lookback_days = 91 | Validation fails with descriptive error |
| 7 | Order update: unknown fields in JSON | Parsed without error (serde default) |
| 8 | Notification: HTML with `<`, `>` in text | `strip_html_tags` handles correctly |
