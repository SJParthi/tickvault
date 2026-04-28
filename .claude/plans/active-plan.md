# Implementation Plan: Wave-1-3 Live Hotfixes (4 bugs caught at 12:49 IST 2026-04-28)

**Status:** PARTIALLY-VERIFIED (items 1-5, 6, 7, 9, 10 shipped; item 8 Docker/Grafana deferred to follow-up commit)
**Date:** 2026-04-28
**Approved by:** Parthiban (operator) — verbatim "fix everything"
**Branch:** `claude/verify-waves-status-HQP6A`
**Triggering incident:** Live observation at 12:49 IST 2026-04-28: boot_audit DDL failed, phase2_audit insert failed (nanos→micros), SLO-02 Telegram 400 Bad Request, Grafana false-healthy.

## Plan Items

- [x] **1. Add `ts` to DEDUP keys for boot_audit + selftest_audit** (fixes AUDIT-04 + missing selftest writes)
  - Files: crates/storage/src/boot_audit_persistence.rs
  - Files: crates/storage/src/selftest_audit_persistence.rs
  - Tests: test_dedup_key_includes_designated_timestamp

- [x] **2. Convert nanos to micros in all 6 audit INSERTs** (fixes AUDIT-01 across the family)
  - Files: crates/storage/src/boot_audit_persistence.rs
  - Files: crates/storage/src/selftest_audit_persistence.rs
  - Files: crates/storage/src/phase2_audit_persistence.rs
  - Files: crates/storage/src/ws_reconnect_audit_persistence.rs
  - Files: crates/storage/src/depth_rebalance_audit_persistence.rs
  - Files: crates/storage/src/order_audit_persistence.rs
  - Tests: test_insert_sql_uses_microseconds_not_nanoseconds

- [x] **3. Strip raw `<` from RealtimeGuaranteeCritical body** (fixes TELEGRAM-01 mid-batch failures)
  - Files: crates/core/src/notification/events.rs
  - Tests: test_realtime_guarantee_critical_body_has_no_unescaped_html_brackets
  - Tests: test_realtime_guarantee_degraded_body_has_no_unescaped_html_brackets
  - Tests: test_realtime_guarantee_healthy_body_has_no_unescaped_html_brackets

- [x] **4. Add HTTP health probe for Grafana** (fixes false-healthy at boot)
  - Files: crates/app/src/infra.rs
  - Tests: test_grafana_health_probe_polls_api_health_endpoint

- [x] **5. Python depth-200 sidecar bridge v0** (replaces / supplements Rust depth-200 client which Dhan resets on ATM/expiry-day)
  - Files: scripts/depth_200_bridge.py
  - Files: scripts/depth_200_bridge_requirements.txt
  - Files: scripts/depth_200_bridge_README.md
  - Files: scripts/test_depth_200_bridge.py
  - Tests: test_parses_bid_frame_into_levels
  - Tests: test_parses_ask_frame
  - Tests: test_disconnect_frame_returns_none
  - Tests: test_zero_priced_levels_are_skipped
  - Tests: test_ilp_line_format_matches_questdb_schema
  - Tests: test_levels_are_one_indexed

- [x] **6. Rust DepthBridgeStateWriter** (writes data/depth-200-bridge/state.json atomically at boot + on rebalance Swap200)
  - Files: crates/app/src/depth_bridge_state_writer.rs
  - Files: crates/app/src/main.rs
  - Tests: test_atomic_write_via_tempfile_rename
  - Tests: test_version_monotonic_increment
  - Tests: test_state_writer_wired_at_boot_and_rebalance

- [x] **7. Prometheus metrics endpoint in Python bridge** (frames_total, reconnects_total, ilp_writes_total, state_reloads_total, active_subs, state_version)
  - Files: scripts/depth_200_bridge.py
  - Files: scripts/test_depth_200_bridge.py
  - Tests: test_metrics_endpoint_serves_prometheus_format
  - Tests: test_counters_increment_on_frame_parse
  - Tests: test_state_version_gauge_tracks_reloads

- [ ] **8. Docker compose service + Grafana panel + Prometheus alert rule for the bridge**
  - Files: deploy/docker/docker-compose.yml
  - Files: deploy/docker/prometheus/alerts.yml
  - Files: deploy/docker/prometheus/prometheus.yml
  - Files: deploy/docker/grafana/dashboards/depth-200-bridge.json
  - Tests: test_alerts_yml_contains_depth_200_bridge_rules
  - Tests: test_prometheus_yml_scrapes_depth_200_bridge

- [x] **9. Disk-full pre-flight check before tick spill writes** (closes single highest-risk hole in zero-loss chain)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_spill_aborts_when_free_bytes_below_threshold

- [x] **10. Lift depth-200 60-attempt cap** (use WS-GAP-04 sleep gate pattern — never give up in-market, sleep until next open out-of-market)
  - Files: crates/core/src/websocket/depth_connection.rs
  - Tests: test_depth_200_never_gives_up_in_market_hours
  - Tests: test_depth_200_sleeps_until_next_open_after_close

## Verification

```bash
cargo check -p tickvault-storage -p tickvault-core -p tickvault-app
cargo test -p tickvault-storage --lib
cargo test -p tickvault-core --lib notification::events
cargo test -p tickvault-app --lib infra
```

All commands pass on commit. 38 audit-module + 3 SLO-body + 1 Grafana-probe tests green.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 12:49 IST | `boot_audit` DDL returns 200; `audit table ready` log emitted |
| 2 | Phase 2 dispatch at 12:50 | `phase2_audit` row inserted, no "timestamp beyond 9999-12-31" |
| 3 | SLO score crosses 0.80 | Telegram message delivered (no 400 Bad Request from `<` in body) |
| 4 | Cold boot, Grafana takes 8s to serve HTTP | App waits for HTTP-200 from `/api/health` before logging "service is healthy" |
