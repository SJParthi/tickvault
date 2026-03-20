# Implementation Plan: 100% Test Coverage Across All 6 Crates

**Status:** DRAFT
**Date:** 2026-03-20
**Approved by:** pending

## Context

Current coverage: 88.69% overall. Target: 100% on all 6 crates.
Total uncovered lines: ~3,781. Estimated new tests: ~300.
Organized into 10 batches by crate and priority.

---

## Batch 1: Common Crate — instrument_types.rs (80% → 100%)

- [ ] Add tests for all rkyv serialization roundtrips (NaiveDateAsI32, DateTimeFixedOffsetAsI64, DurationAsMillis)
  - Files: crates/common/src/instrument_types.rs
  - Tests: test_rkyv_naive_date_roundtrip, test_rkyv_datetime_roundtrip, test_rkyv_duration_roundtrip

- [ ] Add Display impl tests for all enum variants (UnderlyingKind, DhanInstrumentKind, IndexCategory, IndexSubcategory)
  - Files: crates/common/src/instrument_types.rs
  - Tests: test_underlying_kind_display_all, test_dhan_instrument_kind_display_all, test_index_category_display_all, test_index_subcategory_display_all

- [ ] Add tests for NaiveDateRkyvError and DateTimeRkyvError Display/Error impls
  - Files: crates/common/src/instrument_types.rs
  - Tests: test_naive_date_rkyv_error_display, test_datetime_rkyv_error_display

- [ ] Add tests for archived From conversions and ConstituencyBuildMetadata::default
  - Files: crates/common/src/instrument_types.rs
  - Tests: test_archived_underlying_kind_from, test_archived_instrument_kind_from, test_constituency_build_metadata_default

- [ ] Add test for FnoUniverse::derivative_security_ids_for_symbol
  - Files: crates/common/src/instrument_types.rs
  - Tests: test_derivative_security_ids_for_symbol_filters_correctly

## Batch 2: Common Crate — types.rs, trading_calendar.rs, instrument_registry.rs (92-95% → 100%)

- [ ] Add tests for InstrumentType::dhan_api_instrument_type all 6 combinations
  - Files: crates/common/src/types.rs
  - Tests: test_dhan_api_instrument_type_futidx, test_dhan_api_instrument_type_futstk, test_dhan_api_instrument_type_optidx, test_dhan_api_instrument_type_optstk

- [ ] Add tests for archived From conversions (Exchange, ExchangeSegment, OptionType)
  - Files: crates/common/src/types.rs
  - Tests: test_archived_exchange_from, test_archived_exchange_segment_from, test_archived_option_type_from

- [ ] Add test for TradingCalendar::all_entries with holidays and muhurat
  - Files: crates/common/src/trading_calendar.rs
  - Tests: test_all_entries_mixed_holidays_muhurat, test_all_entries_sorted_by_date

- [ ] Add tests for InstrumentRegistry category counting and archived instrument builders
  - Files: crates/common/src/instrument_registry.rs
  - Tests: test_category_counts_multiple_categories, test_make_stock_equity_from_archived, test_make_derivative_from_archived

## Batch 3: API Crate (95% → 100%)

- [ ] Add instruments handler tests: concurrent rebuild guard, error paths, diagnostic endpoint
  - Files: crates/api/src/handlers/instruments.rs
  - Tests: test_rebuild_concurrent_rejection, test_rebuild_error_response, test_rebuild_already_built_today, test_instrument_diagnostic_success

- [ ] Add quote handler tests: QuestDB reachability, JSON parsing edge cases, client build error
  - Files: crates/api/src/handlers/quote.rs
  - Tests: test_quote_questdb_unreachable, test_quote_empty_dataset, test_quote_missing_fields_defaults, test_quote_client_build_error

- [ ] Add middleware auth tests: disabled auth, invalid token, missing header, malformed header
  - Files: crates/api/src/middleware.rs
  - Tests: test_auth_disabled_allows_request, test_auth_invalid_token_401, test_auth_missing_header_401, test_auth_malformed_header_401

- [ ] Add lib.rs CORS fallback test and remaining handler edge cases
  - Files: crates/api/src/lib.rs, crates/api/src/handlers/top_movers.rs, crates/api/src/handlers/index_constituency.rs
  - Tests: test_cors_invalid_origins_fallback, test_top_movers_from_snapshot, test_constituency_unknown_index_404

## Batch 4: Trading Crate — OMS (89-98% → 100%)

- [ ] Add api_client.rs HTTP error matrix: 400, 401, 403, 429, 500, 502, 503 for all endpoints
  - Files: crates/trading/src/oms/api_client.rs
  - Tests: test_place_order_http_400, test_place_order_http_429_rate_limit, test_modify_order_http_500, test_cancel_order_transport_error, test_get_positions_http_401, test_exit_all_malformed_json, test_margin_calc_error, test_fund_limit_error

- [ ] Add engine.rs live mode tests, order update re-indexing, disclosed qty boundary
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_place_order_live_mode, test_order_update_reindexing, test_disclosed_qty_30_percent_boundary, test_rate_limit_cb_interaction, test_validate_sl_trigger

- [ ] Add circuit_breaker.rs recovery logging, CAS race, HalfOpen timing
  - Files: crates/trading/src/oms/circuit_breaker.rs
  - Tests: test_cb_recovery_log_emitted, test_cb_half_open_probe_gate, test_cb_concurrent_failures

- [ ] Add reconciliation.rs warning branches (unknown status, ghost orders, missing from OMS)
  - Files: crates/trading/src/oms/reconciliation.rs
  - Tests: test_reconcile_unknown_dhan_status, test_reconcile_ghost_order, test_reconcile_missing_from_oms

## Batch 5: Trading Crate — Strategy & Risk (88-99% → 100%)

- [ ] Add hot_reload.rs error paths: watcher init failure, invalid TOML recovery, receiver dropped
  - Files: crates/trading/src/strategy/hot_reload.rs
  - Tests: test_hot_reload_invalid_toml_keeps_old, test_hot_reload_file_deleted_during_watch, test_hot_reload_receiver_dropped

- [ ] Add evaluator.rs FSM edge cases: bounds check, trailing stop math, zero ATR, dual entry
  - Files: crates/trading/src/strategy/evaluator.rs
  - Tests: test_eval_out_of_bounds_security_id, test_eval_trailing_stop_crossing, test_eval_zero_atr, test_eval_dual_entry_conditions

- [ ] Add config.rs validation boundaries: position_size_fraction bounds, empty entry conditions
  - Files: crates/trading/src/strategy/config.rs
  - Tests: test_config_position_size_zero, test_config_position_size_above_one, test_config_no_entry_conditions

- [ ] Add risk engine edge cases: halt idempotency, NaN price rejection, position reversal
  - Files: crates/trading/src/risk/engine.rs, crates/trading/src/risk/tick_gap_tracker.rs
  - Tests: test_halt_idempotent, test_nan_price_rejected, test_position_reversal_boundary, test_warmup_boundary_exact, test_counter_saturation

- [ ] Add indicator engine edge cases: out-of-bounds security_id, warmup saturation, zero volume
  - Files: crates/trading/src/indicator/engine.rs
  - Tests: test_indicator_out_of_bounds, test_warmup_saturation, test_zero_volume_vwap_skip

## Batch 6: Core Crate — Parsers & Pipeline (92-99% → 100%)

- [ ] Add tick_processor.rs tests: dedup ring, junk filtering, persist window, broadcast, metrics
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: test_dedup_ring_first_not_dup, test_dedup_ring_identical_dup, test_junk_tick_nan_ltp, test_junk_tick_negative_ltp, test_persist_window_outside_hours, test_parse_error_cap_at_100, test_depth_prices_finite

- [ ] Add parser edge case tests for all parsers with <100% coverage
  - Files: crates/core/src/parser/header.rs, parser/ticker.rs, parser/quote.rs, parser/full_packet.rs, parser/market_depth.rs, parser/deep_depth.rs, parser/oi.rs, parser/previous_close.rs, parser/disconnect.rs, parser/types.rs, parser/market_status.rs, parser/dispatcher.rs
  - Tests: test_header_unknown_exchange_byte, test_ticker_nan_ltp, test_quote_max_values, test_full_all_depth_invalid, test_dispatcher_stacked_frames

- [ ] Add top_movers edge cases and connection_pool capacity exceeded
  - Files: crates/core/src/pipeline/top_movers.rs, crates/core/src/websocket/connection_pool.rs
  - Tests: test_top_movers_invalid_day_close, test_connection_pool_capacity_exceeded, test_total_instruments_sums

## Batch 7: Core Crate — WebSocket & Network (50-96% → 100%)

- [ ] Add order_update_connection.rs tests: error paths, reconnection, market hours, token states
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: test_order_update_no_token, test_order_update_tls_error, test_order_update_connect_refused, test_order_update_read_timeout, test_order_update_non_text_ignored, test_is_within_market_hours

- [ ] Add ip_verifier.rs tests: all error paths, fallback, SSM whitespace, invalid IPv4
  - Files: crates/core/src/network/ip_verifier.rs
  - Tests: test_ssm_whitespace_fails, test_primary_fails_fallback, test_both_exhausted, test_http_error_status, test_invalid_ip_response

- [ ] Add ip_monitor.rs tests: background loop, check pass/fail, fetch errors
  - Files: crates/core/src/network/ip_monitor.rs
  - Tests: test_monitor_check_passes, test_monitor_mismatch_alert, test_fetch_ip_fails, test_fetch_ip_non_success

- [ ] Add connection.rs tests: IDX_I partition, clean shutdown, error variants, reconnection
  - Files: crates/core/src/websocket/connection.rs
  - Tests: test_connection_idx_partition, test_connection_clean_shutdown, test_connection_non_reconnectable, test_connection_reconnect_retry

- [ ] Add tls.rs error path and notification/service.rs error paths
  - Files: crates/core/src/websocket/tls.rs, crates/core/src/notification/service.rs
  - Tests: test_tls_no_root_ca, test_notification_bot_token_missing, test_notification_send_error_logged

## Batch 8: Core Crate — Instruments (92-97% → 100%)

- [ ] Add validation.rs tests for all 6 checks (must-exist indices/equities/stocks, stock count, non-empty, orphans)
  - Files: crates/core/src/instrument/validation.rs
  - Tests: test_validate_indices_missing, test_validate_indices_price_id_mismatch, test_validate_equities_wrong_variant, test_validate_fno_stocks_missing, test_validate_stock_count_bounds, test_validate_empty_underlyings, test_validate_orphan_derivatives

- [ ] Add universe_builder.rs tests for 5-pass pipeline edge cases
  - Files: crates/core/src/instrument/universe_builder.rs
  - Tests: test_build_index_lookup_filters, test_build_equity_lookup_filters, test_discover_fno_skips_test, test_discover_fno_skips_bse_futstk, test_discover_fno_dedup_first_wins

- [ ] Add subscription_planner.rs routing and capacity tests
  - Files: crates/core/src/instrument/subscription_planner.rs
  - Tests: test_plan_idx_i_ticker_only, test_plan_respects_capacity, test_plan_full_mode_depth

## Batch 9: Storage Crate (37-94% → 100%)

- [ ] Add candle_persistence.rs tests: writer new/append/flush, LiveCandleWriter, DDL
  - Files: crates/storage/src/candle_persistence.rs
  - Tests: test_candle_writer_new_unreachable, test_append_candle_batch_flush, test_force_flush_resets, test_live_candle_oi_zero, test_live_candle_all_segments, test_ensure_candle_table_create, test_ensure_candle_table_dedup

- [ ] Add constituency_persistence.rs tests: DDL, persist retry, inner logic
  - Files: crates/storage/src/constituency_persistence.rs
  - Tests: test_ensure_constituency_table_create, test_ensure_constituency_table_dedup, test_persist_constituency_retry, test_persist_constituency_all_fail_ok, test_persist_inner_security_id_zero

- [ ] Add calendar_persistence.rs tests: DDL, persist retry, inner logic
  - Files: crates/storage/src/calendar_persistence.rs
  - Tests: test_ensure_calendar_table_create, test_persist_calendar_retry, test_persist_inner_holiday_vs_muhurat, test_persist_inner_midnight_timestamp

- [ ] Add valkey_cache.rs tests: all async methods with mock or real Valkey
  - Files: crates/storage/src/valkey_cache.rs
  - Tests: test_valkey_health_check, test_valkey_get_missing, test_valkey_set_get_roundtrip, test_valkey_set_ex_ttl, test_valkey_del, test_valkey_exists, test_valkey_set_nx_ex

- [ ] Add materialized_views.rs tests: DDL execution, view creation, partial failure
  - Files: crates/storage/src/materialized_views.rs
  - Tests: test_ensure_views_creates_base_table, test_ensure_views_enables_dedup, test_execute_ddl_success, test_execute_ddl_http_400

- [ ] Add tick_persistence.rs remaining tests: build_tick_row, depth rows, prev_close, DDL
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_build_tick_row_timestamps, test_build_tick_row_f32_to_f64, test_build_depth_rows_5_levels, test_build_prev_close_row, test_ensure_tick_table_ddl, test_ensure_depth_prev_close_tables

- [ ] Add instrument_persistence.rs remaining tests: DDL, write functions, timestamps
  - Files: crates/storage/src/instrument_persistence.rs
  - Tests: test_ensure_instrument_tables_4_created, test_build_snapshot_timestamp, test_write_build_metadata, test_write_underlyings, test_write_derivative_contracts, test_write_subscribed_indices

## Batch 10: App Crate (8% → 100%)

- [ ] Add pure helper function tests: format_timeframe_details, format_violation_details, format_cross_match_details, create_log_file_writer, compute_market_close_sleep remaining branches
  - Files: crates/app/src/main.rs
  - Tests: test_format_timeframe_details, test_format_violation_details, test_format_cross_match_details, test_create_log_file_writer_success, test_create_log_file_writer_permission_denied, test_compute_market_close_sleep_all_branches

- [ ] Add TokenHandleBridge tests and init_trading_pipeline tests
  - Files: crates/app/src/trading_pipeline.rs
  - Tests: test_token_bridge_valid, test_token_bridge_empty, test_token_bridge_none, test_init_pipeline_no_config, test_init_pipeline_invalid_toml, test_init_pipeline_valid

- [ ] Add observability init tests: metrics disabled/enabled, tracing disabled/enabled
  - Files: crates/app/src/observability.rs
  - Tests: test_init_metrics_disabled, test_init_metrics_enabled, test_init_tracing_disabled, test_init_tracing_invalid_endpoint

- [ ] Add infra.rs tests: is_service_reachable, wait_for_service_healthy, run_docker_compose_up, open_in_browser, is_docker_daemon_running
  - Files: crates/app/src/infra.rs
  - Tests: test_service_reachable_loopback, test_service_unreachable, test_wait_healthy_immediate, test_wait_healthy_timeout, test_docker_compose_success, test_docker_compose_failure, test_open_in_browser_platform

- [ ] Add load_instruments tests with mocked instrument loader
  - Files: crates/app/src/main.rs
  - Tests: test_load_instruments_fresh_build, test_load_instruments_cached_plan, test_load_instruments_unavailable

- [ ] Add main.rs boot path tests with comprehensive service mocking
  - Files: crates/app/src/main.rs
  - Tests: test_fast_boot_valid_cache, test_slow_boot_sequence, test_two_phase_boot_decision

- [ ] Add consumer tests: tick persistence consumer, candle persistence consumer
  - Files: crates/app/src/main.rs
  - Tests: test_tick_consumer_flush_on_batch, test_tick_consumer_shutdown, test_candle_consumer_aggregation, test_candle_consumer_flush

- [ ] Add shutdown orchestration tests and historical candle fetch tests
  - Files: crates/app/src/main.rs
  - Tests: test_shutdown_ctrl_c, test_shutdown_market_close, test_historical_fetch_trading_day, test_historical_fetch_non_trading_day

- [ ] Add run_trading_pipeline tests with full mocked subsystems
  - Files: crates/app/src/trading_pipeline.rs
  - Tests: test_pipeline_tick_to_order, test_pipeline_order_update, test_pipeline_strategy_hot_reload, test_pipeline_shutdown

- [ ] Add ensure_infra_running integration test with mocked SSM and Docker
  - Files: crates/app/src/infra.rs
  - Tests: test_ensure_infra_questdb_already_up, test_ensure_infra_docker_not_running, test_ensure_infra_ssm_fails

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | cargo llvm-cov --workspace | All 6 crates at 100% line coverage |
| 2 | cargo test --workspace | All existing + new tests pass |
| 3 | cargo clippy --workspace -- -D warnings | No new warnings |
| 4 | cargo fmt --check | Formatted correctly |
| 5 | make quality | Full CI pipeline passes |
