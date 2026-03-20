# Implementation Plan: Achieve 100% Test Coverage Across All 6 Crates

**Status:** DRAFT
**Date:** 2026-03-20
**Approved by:** pending

## Context

Current coverage: **88.68%** (region) / **92.17%** (function) / **88.02%** (line).
Target: **100%** across all crates and all coverage metrics.
Uncovered: ~6,725 regions across ~40 files in 6 crates.

## Strategy

Work crate-by-crate, bottom-up (common → core → trading → storage → api → app).
For each file:
1. Run `cargo llvm-cov` per-crate to identify exact uncovered lines
2. Write tests targeting those specific lines
3. Re-run coverage to verify 100%

For infrastructure code that makes external calls (HTTP, WebSocket, Docker, QuestDB):
- Extract testable logic into pure functions where possible
- Use mock-based tests with trait abstractions
- Test error handling paths via simulated failures

## Plan Items

### Batch 1: common crate (3 files, ~95-97% → 100%)

- [ ] Cover remaining branches in `common/src/types.rs` (ExchangeSegment, FeedResponseCode edge cases)
  - Files: crates/common/src/types.rs
  - Tests: test_all_exchange_segment_display, test_unknown_feed_response_codes

- [ ] Cover remaining branches in `common/src/instrument_types.rs` (rkyv edge cases, NaiveDate boundaries)
  - Files: crates/common/src/instrument_types.rs
  - Tests: test_instrument_record_edge_cases, test_option_type_none_handling

- [ ] Cover remaining branches in `common/src/trading_calendar.rs` (long holiday stretches, next_trading_day boundaries)
  - Files: crates/common/src/trading_calendar.rs
  - Tests: test_next_trading_day_long_holiday, test_weekend_boundary_cases

### Batch 2: core crate — auth module (5 files, ~92-99% → 100%)

- [ ] Cover `core/src/auth/secret_manager.rs` error paths (SSM fetch failures, env validation)
  - Files: crates/core/src/auth/secret_manager.rs
  - Tests: test_ssm_fetch_error_paths, test_env_validation_edge_cases

- [ ] Cover `core/src/auth/token_cache.rs` error paths (hash mismatch, timestamp parse, file errors)
  - Files: crates/core/src/auth/token_cache.rs
  - Tests: test_cache_corruption_recovery, test_timestamp_parse_failures

- [ ] Cover `core/src/auth/token_manager.rs` remaining paths (HTTPS validation, retry exhaustion)
  - Files: crates/core/src/auth/token_manager.rs
  - Tests: test_https_validation_error, test_retry_exhaustion

- [ ] Cover `core/src/auth/totp_generator.rs` error paths (secret encoding edge cases)
  - Files: crates/core/src/auth/totp_generator.rs
  - Tests: test_totp_init_error, test_secret_encoding_edge

- [ ] Cover `core/src/auth/types.rs` remaining branches
  - Files: crates/core/src/auth/types.rs
  - Tests: test_auth_types_serialization_variants

### Batch 3: core crate — instrument module (7 files, ~53-99% → 100%)

- [ ] Cover `core/src/instrument/diagnostic.rs` (URL reachability failures, CSV parse errors)
  - Files: crates/core/src/instrument/diagnostic.rs
  - Tests: test_diagnostic_url_timeout, test_diagnostic_csv_parse_failure, test_diagnostic_universe_validation_error

- [ ] Cover `core/src/instrument/binary_cache.rs` error paths (serialization, file I/O, rkyv validation)
  - Files: crates/core/src/instrument/binary_cache.rs
  - Tests: test_binary_cache_deser_error, test_binary_cache_file_io_error

- [ ] Cover `core/src/instrument/daily_scheduler.rs` (disabled config, non-trading day, channel errors)
  - Files: crates/core/src/instrument/daily_scheduler.rs
  - Tests: test_scheduler_disabled, test_scheduler_non_trading_day, test_scheduler_channel_closed

- [ ] Cover `core/src/instrument/csv_downloader.rs` remaining paths (timeout, redirect)
  - Files: crates/core/src/instrument/csv_downloader.rs
  - Tests: test_csv_download_timeout, test_csv_download_redirect

- [ ] Cover `core/src/instrument/subscription_planner.rs` edge cases
  - Files: crates/core/src/instrument/subscription_planner.rs
  - Tests: test_planner_max_instruments_boundary

- [ ] Cover `core/src/instrument/universe_builder.rs` edge cases
  - Files: crates/core/src/instrument/universe_builder.rs
  - Tests: test_universe_empty_csv, test_universe_all_filtered

- [ ] Cover `core/src/instrument/delta_detector.rs` any remaining regions
  - Files: crates/core/src/instrument/delta_detector.rs
  - Tests: test_delta_detector_remaining_branches

### Batch 4: core crate — historical module (2 files, ~37-45% → 100%)

- [ ] Cover `core/src/historical/candle_fetcher.rs` (error classification, retry waves, token refresh, HTTP errors)
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Tests: test_classify_error_all_branches, test_retry_wave_logic, test_token_refresh_wait, test_http_error_handling, test_consecutive_failure_break

- [ ] Cover `core/src/historical/cross_verify.rs` (OHLC violation detection, tolerance logic, missing views)
  - Files: crates/core/src/historical/cross_verify.rs
  - Tests: test_ohlc_violation_detection, test_price_tolerance_epsilon, test_volume_tolerance_pct, test_missing_materialized_view

### Batch 5: core crate — network module (2 files, ~63-79% → 100%)

- [ ] Cover `core/src/network/ip_verifier.rs` (SSM failure, IP detection failures, Dhan API errors)
  - Files: crates/core/src/network/ip_verifier.rs
  - Tests: test_ssm_fetch_failure, test_ip_detection_both_fail, test_ip_mismatch, test_dhan_api_errors

- [ ] Cover `core/src/network/ip_monitor.rs` (monitor loop, check failures, fallback logic)
  - Files: crates/core/src/network/ip_monitor.rs
  - Tests: test_ip_mismatch_detection, test_primary_fail_fallback_success, test_both_checks_fail

### Batch 6: core crate — websocket module (3 files, ~50-97% → 100%)

- [ ] Cover `core/src/websocket/order_update_connection.rs` (disconnect paths, TLS failures, message types)
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: test_order_ws_tls_failure, test_order_ws_disconnect_handling, test_order_ws_non_text_messages, test_order_ws_market_hours_boundary

- [ ] Cover `core/src/websocket/connection.rs` remaining paths (TLS handshake, disconnection)
  - Files: crates/core/src/websocket/connection.rs
  - Tests: test_ws_tls_handshake_failure, test_ws_disconnect_edge_cases

- [ ] Cover `core/src/websocket/tls.rs` remaining paths (certificate failures, cipher negotiation)
  - Files: crates/core/src/websocket/tls.rs
  - Tests: test_tls_cert_load_failure, test_tls_cipher_edge_cases

### Batch 7: core crate — other modules (3 files)

- [ ] Cover `core/src/notification/service.rs` (Telegram/SNS error paths, retry logic)
  - Files: crates/core/src/notification/service.rs
  - Tests: test_telegram_api_error, test_sns_failure, test_notification_retry

- [ ] Cover `core/src/index_constituency/mod.rs` (0% — download failure, cache fallback)
  - Files: crates/core/src/index_constituency/mod.rs
  - Tests: test_constituency_download_failure_cache_fallback, test_constituency_cache_miss, test_constituency_parse_failures

- [ ] Cover `core/src/index_constituency/csv_downloader.rs` (17% — HTTP errors, retry logic, concurrent downloads)
  - Files: crates/core/src/index_constituency/csv_downloader.rs
  - Tests: test_download_http_404, test_download_timeout, test_download_retry_backoff, test_concurrent_download_semaphore

- [ ] Cover `core/src/test_support.rs` (76% — env var branches, credentials file paths)
  - Files: crates/core/src/test_support.rs
  - Tests: test_has_aws_credentials_no_env, test_has_aws_credentials_no_file

### Batch 8: trading crate (8 files, ~89-99% → 100%)

- [ ] Cover `trading/src/oms/api_client.rs` (HTTP error responses, JSON parse errors, auth failures)
  - Files: crates/trading/src/oms/api_client.rs
  - Tests: test_api_client_http_errors, test_api_client_json_parse_error, test_api_client_auth_failure

- [ ] Cover `trading/src/oms/engine.rs` (modify/cancel branches, reconciliation, dry-run/live mode)
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_engine_modify_order, test_engine_cancel_order, test_engine_reconciliation_path, test_engine_dry_run_vs_live

- [ ] Cover `trading/src/oms/circuit_breaker.rs` (half-open probe, state edge cases)
  - Files: crates/trading/src/oms/circuit_breaker.rs
  - Tests: test_half_open_probe_mechanics, test_circuit_breaker_state_edges

- [ ] Cover `trading/src/oms/rate_limiter.rs` remaining paths
  - Files: crates/trading/src/oms/rate_limiter.rs
  - Tests: test_rate_limiter_saturation_edge

- [ ] Cover `trading/src/oms/reconciliation.rs` remaining paths (ghost orders, unknown status)
  - Files: crates/trading/src/oms/reconciliation.rs
  - Tests: test_ghost_order_detection, test_unknown_status_parsing

- [ ] Cover `trading/src/strategy/config.rs` (TOML parse failures, validation edge cases)
  - Files: crates/trading/src/strategy/config.rs
  - Tests: test_strategy_config_invalid_toml, test_strategy_config_validation_failures

- [ ] Cover `trading/src/strategy/evaluator.rs` (complex evaluation branches, indicator edge cases)
  - Files: crates/trading/src/strategy/evaluator.rs
  - Tests: test_evaluator_complex_branches, test_evaluator_indicator_edge_cases

- [ ] Cover `trading/src/strategy/hot_reload.rs` (file watcher errors, config reload failures, race conditions)
  - Files: crates/trading/src/strategy/hot_reload.rs
  - Tests: test_hot_reload_watcher_error, test_hot_reload_invalid_config, test_hot_reload_race_condition

- [ ] Cover `trading/src/risk/engine.rs` remaining regions (if any)
  - Files: crates/trading/src/risk/engine.rs
  - Tests: test_risk_engine_remaining_branches

- [ ] Cover `trading/src/risk/tick_gap_tracker.rs` remaining paths
  - Files: crates/trading/src/risk/tick_gap_tracker.rs
  - Tests: test_tick_gap_remaining_edges

- [ ] Cover `trading/src/indicator/engine.rs` remaining region
  - Files: crates/trading/src/indicator/engine.rs
  - Tests: test_indicator_rare_error_states

### Batch 9: storage crate (6 files, ~37-94% → 100%)

- [ ] Cover `storage/src/candle_persistence.rs` (38% — ILP operations, timestamp conversion, flush logic)
  - Files: crates/storage/src/candle_persistence.rs
  - Tests: test_candle_persist_timestamp_offset, test_candle_persist_ilp_row_building, test_candle_persist_flush_trigger, test_candle_persist_connection_error

- [ ] Cover `storage/src/constituency_persistence.rs` (37% — DDL, retry logic, ILP construction)
  - Files: crates/storage/src/constituency_persistence.rs
  - Tests: test_constituency_ddl_creation, test_constituency_retry_logic, test_constituency_ilp_construction

- [ ] Cover `storage/src/calendar_persistence.rs` (58% — HTTP errors, ILP failures, retry)
  - Files: crates/storage/src/calendar_persistence.rs
  - Tests: test_calendar_http_error_paths, test_calendar_ilp_failures, test_calendar_retry_backoff

- [ ] Cover `storage/src/valkey_cache.rs` (62% — pool exhaustion, redis errors, health check)
  - Files: crates/storage/src/valkey_cache.rs
  - Tests: test_valkey_pool_exhaustion, test_valkey_redis_errors, test_valkey_unhealthy_ping

- [ ] Cover `storage/src/materialized_views.rs` (83% — DDL errors, view creation edge cases)
  - Files: crates/storage/src/materialized_views.rs
  - Tests: test_view_creation_error_paths, test_view_ddl_edge_cases

- [ ] Cover `storage/src/tick_persistence.rs` remaining paths (ILP write errors, dedup edge cases)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_tick_persist_ilp_write_error, test_tick_persist_dedup_edge

- [ ] Cover `storage/src/instrument_persistence.rs` remaining paths (DDL errors, snapshot timestamps)
  - Files: crates/storage/src/instrument_persistence.rs
  - Tests: test_instrument_persist_ddl_error, test_instrument_snapshot_timestamp_edge

### Batch 10: api crate (4 files, ~62-99% → 100%)

- [ ] Cover `api/src/handlers/instruments.rs` (62% — network failures, concurrent rebuild)
  - Files: crates/api/src/handlers/instruments.rs
  - Tests: test_instruments_network_failure, test_instruments_concurrent_rebuild_rejection

- [ ] Cover `api/src/handlers/quote.rs` (85% — error branches, empty dataset, malformed JSON)
  - Files: crates/api/src/handlers/quote.rs
  - Tests: test_quote_error_branches, test_quote_empty_dataset, test_quote_malformed_json

- [ ] Cover `api/src/middleware.rs` remaining paths (env var parsing, malformed auth header)
  - Files: crates/api/src/middleware.rs
  - Tests: test_middleware_env_parse_failure, test_middleware_malformed_auth

- [ ] Cover `api/src/state.rs` remaining atomic operations
  - Files: crates/api/src/state.rs
  - Tests: test_state_atomic_edge_cases

### Batch 11: app crate (3 files, ~0-16% → 100%)

- [ ] Cover `app/src/infra.rs` (16% — Docker orchestration, health probes, browser open)
  - Files: crates/app/src/infra.rs
  - Tests: test_infra_docker_unavailable, test_infra_service_reachable, test_infra_health_timeout, test_infra_browser_open_linux

- [ ] Cover `app/src/main.rs` helper functions (5% — format helpers, compute functions, load_instruments)
  - Files: crates/app/src/main.rs
  - Tests: test_compute_market_close_sleep, test_format_timeframe_details, test_format_violation_details, test_format_cross_match_details, test_load_instruments_branches

- [ ] Cover `app/src/main.rs` boot paths (fast boot, slow boot, shutdown — extract testable logic)
  - Files: crates/app/src/main.rs
  - Tests: test_fast_boot_path_logic, test_slow_boot_path_logic, test_shutdown_orchestration

- [ ] Cover `app/src/trading_pipeline.rs` (0% — token bridge, signal handling, pipeline init)
  - Files: crates/app/src/trading_pipeline.rs
  - Tests: test_token_handle_bridge, test_signal_enter_long, test_signal_enter_short, test_signal_exit, test_init_trading_pipeline

### Batch 12: Final verification

- [ ] Run `cargo llvm-cov --workspace --summary-only` and verify 100% on ALL files
  - Files: all crates
  - Tests: N/A (verification step)

- [ ] Update `quality/crate-coverage-thresholds.toml` to require 100%
  - Files: quality/crate-coverage-thresholds.toml
  - Tests: N/A

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Run cargo llvm-cov after all batches | 100.00% region coverage |
| 2 | Every file in every crate | 100.00% per-file |
| 3 | All existing tests still pass | 0 failures, 0 regressions |
| 4 | cargo clippy --workspace -- -D warnings | 0 warnings |
| 5 | cargo fmt --check | No formatting issues |
