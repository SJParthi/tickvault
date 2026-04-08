# Implementation Plan: System Audit Fixes (10 items)

**Status:** APPROVED
**Date:** 2026-04-08
**Approved by:** Parthiban (implicit — "fix everything")

## Plan Items

- [ ] 1. Greeks pipeline market hours gate — skip cycles outside 09:15-15:30 IST
  - Files: crates/app/src/greeks_pipeline.rs
  - Tests: test_greeks_market_hours_gate_source_code

- [ ] 2. Historical candle invalid-day log — downgrade WARN to DEBUG for expected boundary skips
  - Files: crates/core/src/historical/candle_fetcher.rs
  - Tests: existing tests pass

- [ ] 3. Depth connection .min(secs_until) no-op bug — remove useless .min()
  - Files: crates/core/src/websocket/depth_connection.rs
  - Tests: test_calculate_secs_until_market_open_*

- [ ] 4. AWS log filter — suppress aws_config::profile::credentials at warn level
  - Files: crates/app/src/main.rs
  - Tests: test_aws_log_filter_source_code

- [ ] 5. Pre-market readiness check at 08:00/08:05 IST
  - Files: crates/app/src/main.rs
  - Tests: test_pre_market_check_source_code

- [ ] 6. Alloy container — ensure data/logs dir exists + improve Alloy config
  - Files: deploy/docker/alloy/alloy-config.alloy, crates/app/src/infra.rs
  - Tests: manual verification

- [ ] 7. Constituency CSV downloader — add User-Agent header for NSE
  - Files: crates/core/src/index_constituency/csv_downloader.rs
  - Tests: existing tests pass

- [ ] 8. Duplicate security_id log level WARN → DEBUG
  - Files: crates/core/src/instrument/universe_builder.rs
  - Tests: existing tests pass

- [ ] 9. Invalid candle count log level WARN → DEBUG for small counts
  - Files: crates/core/src/historical/candle_fetcher.rs (same as item 2)
  - Tests: existing tests pass

- [ ] 10. Build, clippy, fmt, test — verify all pass
  - Files: all modified
  - Tests: cargo test --workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 21:00 IST | Greeks pipeline skips cycles, logs "outside market hours" |
| 2 | Boot at 09:00 IST | Pre-market check at 08:00, greeks runs normally |
| 3 | Depth WS timeout off-hours | Sleep uses correct clamped value |
| 4 | AWS SSM fetch | No access_key_id in INFO logs |
| 5 | Invalid candles | DEBUG log, not WARN |
| 6 | Duplicate security_id | DEBUG log, not WARN |
| 7 | NSE returns HTML for CSV | Better User-Agent header |
