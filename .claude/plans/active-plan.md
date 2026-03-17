# Implementation Plan: Test Coverage Gap Closure (Phase 2 — Deep Scan)

**Status:** VERIFIED
**Date:** 2026-03-17
**Approved by:** Parthiban

## Plan Items (Phase 1 — Completed Previously)

- [x] **1. OMS API Client — HTTP error path tests**
  - Files: `crates/trading/src/oms/api_client.rs`
  - Tests: 14 mock HTTP tests

- [x] **2. Token Manager — lifecycle unit tests**
  - Files: `crates/core/src/auth/token_manager.rs`
  - Tests: 6 lifecycle tests

- [x] **3. Historical Candle Fetcher — edge case tests**
  - Files: `crates/core/src/historical/candle_fetcher.rs`
  - Tests: 9 edge case tests

- [x] **4. Risk Engine — P&L edge case tests**
  - Files: `crates/trading/src/risk/engine.rs`
  - Tests: 14 P&L edge tests

- [x] **5. WebSocket Connection Pool — already comprehensive (32+ tests)**

- [x] **6. Notification Service — helper function tests**
  - Files: `crates/core/src/notification/service.rs`
  - Tests: 11 helper tests

- [x] **7. Config Validation — cross-field and boundary tests**
  - Files: `crates/common/src/config.rs`
  - Tests: 14 boundary tests

- [x] **8. Order Update WebSocket — field mapping tests**
  - Files: `crates/core/src/parser/order_update.rs`
  - Tests: 13 field mapping tests

## Plan Items (Phase 2 — Deep Scan)

- [x] **9. Indicator Engine — O(1) per-tick indicator tests (was 0 tests)**
  - Files: `crates/trading/src/indicator/engine.rs`
  - Tests: `test_new_engine_pre_allocates_state`, `test_new_engine_alpha_values_positive`, `test_fast_alpha_greater_than_slow`, `test_out_of_bounds_security_id_returns_default_snapshot`, `test_first_tick_initializes_ema_to_price`, `test_first_tick_not_warm`, `test_first_tick_rsi_is_neutral`, `test_first_tick_atr_is_zero`, `test_warmup_completes_after_threshold`, `test_ema_converges_to_constant_price`, `test_rsi_all_gains_approaches_100`, `test_rsi_all_losses_approaches_0`, `test_macd_at_constant_price_converges_to_zero`, `test_vwap_with_volume`, `test_vwap_zero_volume_stays_zero`, `test_vwap_reset_daily_clears_accumulators`, `test_bollinger_at_constant_price_bands_collapse`, `test_bollinger_reset_daily`, `test_sma_single_value_equals_price`, `test_sma_within_period_is_exact`, `test_atr_second_tick_initializes_to_true_range`, `test_obv_increases_on_price_up`, `test_obv_decreases_on_price_down`, `test_obv_unchanged_on_same_price`, `test_supertrend_first_tick_sets_initial_bands`, `test_instruments_are_independent`, `test_snapshot_carries_price_context`, `test_params_accessor`, `test_adx_starts_at_zero`, `test_adx_positive_after_trending`

- [x] **10. Indicator Types — RingBuffer, IndicatorState, IndicatorParams, Snapshot tests**
  - Files: `crates/trading/src/indicator/types.rs`
  - Tests: `test_ring_buffer_new_is_empty`, `test_ring_buffer_push_increments_count`, `test_ring_buffer_push_returns_evicted_zero_when_not_full`, `test_ring_buffer_wraps_at_capacity`, `test_ring_buffer_oldest_when_full`, `test_ring_buffer_eviction_order`, `test_ring_buffer_default_same_as_new`, `test_indicator_state_new_all_zeros`, `test_indicator_state_not_warm_initially`, `test_indicator_state_warm_at_threshold`, `test_indicator_state_not_warm_below_threshold`, `test_indicator_state_default_same_as_new`, `test_indicator_params_default_values`, `test_ema_alpha_period_1`, `test_ema_alpha_period_12`, `test_wilder_factor_period_14`, `test_wilder_factor_period_1`, `test_indicator_snapshot_default_all_zeros`, `test_indicator_snapshot_is_copy`, `test_indicator_state_is_copy`

- [x] **11. Storage DEDUP Key Gap Tests (STORAGE-GAP-01, I-P1-05)**
  - Files: `crates/storage/src/tick_persistence.rs`, `crates/storage/src/instrument_persistence.rs`
  - Tests: `test_ticks_ddl_has_segment_column`, `test_tick_dedup_key_is_pinned`, `test_depth_ddl_has_segment_column`, `test_dedup_key_derivative_contracts_includes_security_id`, `test_dedup_key_fno_underlyings_is_underlying_symbol`, `test_dedup_key_build_metadata_is_csv_source`, `test_dedup_key_subscribed_indices_is_security_id`, `test_derivative_contracts_ddl_contains_dedup_column`, `test_fno_underlyings_ddl_contains_dedup_column`, `test_derivative_contracts_ddl_has_underlying_symbol`

- [x] **12. Tick Gap Tracker — edge case tests**
  - Files: `crates/trading/src/risk/tick_gap_tracker.rs`
  - Tests: `out_of_order_timestamp_returns_ok`, `warning_result_carries_gap_seconds`, `error_result_carries_gap_seconds`, `reset_mid_warmup_restarts_from_scratch`, `gap_just_below_warning_threshold_is_ok`, `cumulative_counters_increment`, `zero_capacity_tracker_works`

- [x] **13. Reconciliation — edge case tests**
  - Files: `crates/trading/src/oms/reconciliation.rs`
  - Tests: `epsilon_boundary_fill_mismatch`, `price_beyond_epsilon_triggers_fill_update`, `status_and_fill_mismatch_on_same_order`, `unknown_status_not_counted_in_total_checked`, `all_unknown_statuses_yields_zero_checked`, `terminal_oms_order_missing_from_dhan_not_ghost`, `reconciliation_report_default_all_zeros`

- [x] **14. OMS Engine — dry-run mode + lifecycle tests**
  - Files: `crates/trading/src/oms/engine.rs`
  - Tests: `oms_defaults_to_dry_run`, `dry_run_place_order_returns_paper_id`, `dry_run_sequential_paper_ids`, `dry_run_order_tracked_in_transit`, `handle_update_via_correlation_re_indexes_order`, `multiple_updates_increment_counter`, `handle_update_fills_data`, `reset_daily_also_resets_paper_counter`, `needs_reconciliation_flag_set_on_invalid_transition`

- [x] **15. Common Crate — constants sanity + DhanIntradayResponse deserialization**
  - Files: `crates/common/src/constants.rs`, `crates/common/src/tick_types.rs`
  - Tests: `test_ist_offset_seconds_is_5h30m`, `test_ist_offset_nanos_consistent_with_seconds`, `test_sebi_max_orders_per_second`, `test_sebi_audit_retention_years`, `test_ticker_packet_size`, `test_quote_packet_size`, `test_full_packet_size`, `test_oi_packet_size`, `test_disconnect_packet_size`, `test_exchange_segment_code_6_is_unused`, `test_exchange_segment_sequential_except_gap`, `test_disconnect_codes_match_dhan_spec`, `test_response_code_disconnect_is_50`, `test_max_connections_is_5`, `test_max_instruments_per_connection_is_5000`, `test_subscription_batch_size_within_spec`, `test_total_subscriptions_consistent`, `test_deep_depth_header_is_12_not_8`, `test_deep_depth_bid_ask_codes`, `test_market_open_time`, `test_order_update_login_msg_code_is_42`, `test_indicator_warmup_ticks_positive`, `test_indicator_ring_buffer_capacity_power_of_two`, `test_intraday_response_full_json_deserialize`, `test_intraday_response_volume_as_float_truncated_to_i64`, `test_intraday_response_missing_oi_defaults_empty`, `test_intraday_response_empty_arrays`, `test_market_depth_level_equality`, `test_deep_depth_level_f64_precision_preserved`, `test_parsed_tick_all_fields_settable`

- [x] **16. Candle Persistence — DDL and constant validation**
  - Files: `crates/storage/src/candle_persistence.rs`
  - Tests: `test_dedup_key_candles_is_security_id`, `test_candles_1m_ddl_has_segment_column`, `test_candles_1m_ddl_has_security_id`, `test_candles_1m_ddl_has_ohlcv_columns`, `test_live_candle_flush_batch_size_positive`, `test_questdb_ddl_timeout_is_reasonable_candle`

- [x] **17. Build verification**
  - `cargo test --workspace` — 2492 tests pass, 0 failures
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
| 9 | Indicator engine: out-of-bounds security_id | Returns default snapshot |
| 10 | Indicator engine: VWAP with zero volume | Returns 0.0, no division by zero |
| 11 | Tick gap: out-of-order timestamp | saturating_sub → Ok, no panic |
| 12 | Reconciliation: unknown Dhan status | Skipped, not counted in total_checked |
| 13 | OMS dry-run: place order | Returns PAPER-N ID, no HTTP call |
| 14 | DEDUP_KEY_TICKS | Currently "security_id", DDL has segment column |
