# Implementation Plan: Master Fix List — Branch Review Findings

**Status:** VERIFIED
**Date:** 2026-04-06
**Approved by:** Parthiban (via master fix list document)

## Plan Items

### Critical (Merge Blockers)

- [x] C1: DeepDepthWriter ring buffer + disk spill for flush failure recovery
  - Files: crates/storage/src/deep_depth_persistence.rs
  - Tests: test_deep_depth_ring_buffer_push_pop, test_deep_depth_spill_to_disk_roundtrip, test_deep_depth_ddl_sql_format, test_deep_depth_dedup_keys_format, test_deep_depth_record_serialize_deserialize_roundtrip

- [x] C2: Wire depth_rebalancer into main.rs boot sequence
  - Files: crates/app/src/main.rs
  - Impl: tokio::spawn(run_depth_rebalancer(...)) at Step 8d, spot price updater via tick broadcast

- [x] C3: Exit signal closes open positions, not just cancels orders
  - Files: crates/app/src/trading_pipeline.rs, crates/trading/src/risk/engine.rs
  - Tests: test_net_lots_for_long_position, test_net_lots_for_short_position, test_net_lots_for_flat_position, test_net_lots_for_unknown_security

### High Priority

- [x] H1: Replace std::sync::RwLock with tokio::sync::RwLock in depth_rebalancer
  - Files: crates/core/src/instrument/depth_rebalancer.rs
  - Tests: 8 async tests pass (test_update_and_get_spot_price, test_multiple_underlyings, etc.)

- [x] H3: Add timeout to OMS reqwest::Client + OMS_HTTP_TIMEOUT constant
  - Files: crates/app/src/trading_pipeline.rs, crates/common/src/constants.rs
  - Impl: OMS_HTTP_TIMEOUT_SECS = 5, connect_timeout = 3s, pool_idle_timeout = 90s

- [x] H6: Fix date validation to reject impossible dates (Feb 30, Apr 31)
  - Files: crates/api/src/handlers/option_chain.rs
  - Tests: test_is_valid_date_rejects_feb_30, test_is_valid_date_accepts_leap_year, test_is_valid_date_rejects_malformed

### Observability

- [x] O2+O3+O4: Add missing Prometheus metrics (rebalancer heartbeat, pipeline heartbeat, lagged ticks counter)
  - Files: crates/core/src/instrument/depth_rebalancer.rs, crates/app/src/trading_pipeline.rs
  - Metrics: tv_depth_rebalancer_active, tv_trading_pipeline_heartbeat, tv_trading_pipeline_ticks_lagged_total

- [x] O6-remaining: Add DepthRebalancerDown + TradingPipelineStalled + TradingPipelineLagging alert rules
  - Files: deploy/docker/prometheus/rules/tv-alerts.yml

### Medium Priority

- [x] M1: Log warning when size_fraction ignored (trading_pipeline.rs)
- [x] M3: Use get_mut() to avoid String alloc on spot price update (depth_rebalancer.rs)
- [x] M5: Document CorrelationTracker in-memory limitation (idempotency.rs)
- [x] M8: OMS_HTTP_TIMEOUT_SECS constant (merged with H3)
- [x] M9: SQL safety comment in cross_verify.rs

### Build Verification

- [x] cargo fmt --check: PASS
- [x] cargo clippy (changed crates): PASS
- [x] cargo test: storage 1281, trading 1173, api 236, common 547, app 375 — all PASS

## Scenarios

| # | Scenario | Expected | Status |
|---|----------|----------|--------|
| 1 | DeepDepthWriter flush fails | Data rescued to ring buffer, not lost | IMPLEMENTED |
| 2 | Depth rebalancer running during market | Heartbeat metric emitted every 60s | IMPLEMENTED |
| 3 | Exit signal with open position | Closing market order placed | IMPLEMENTED |
| 4 | OMS API hangs | Times out after 5s | IMPLEMENTED |
| 5 | Date "2026-02-30" passed to option chain | Rejected by validation | IMPLEMENTED |
| 6 | Trading pipeline lags behind ticks | Counter incremented, alert fires | IMPLEMENTED |
