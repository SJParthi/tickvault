# Implementation Plan: Zero Tick Loss Session 2 — Full Backlog

**Status:** VERIFIED
**Date:** 2026-04-13
**Approved by:** Parthiban ("everything bro")
**Branch:** `claude/websocket-zero-tick-loss-nUAqy`
**Session 1 archived:** `archive/2026-04-13-zero-tick-loss-session-1.md`
**Parent backlog:** `backlog-zero-tick-loss.md`

## Goal

Burn down the entire zero-tick-loss backlog: F1 + A3 + A4 + A5 + B2 + B3 + C1 + E1 + E2. Commit after each item so any mid-session interruption leaves a clean tree.

## Plan items

- [x] F1. Parallel-test flake fix via configurable spill_dir.
  - Files: tick_persistence.rs, tick_resilience.rs
  - Tests: test_custom_spill_dir_used_for_spill, test_custom_spill_dir_used_for_dlq, test_prolonged_outage_ring_plus_spill_zero_loss

- [x] A5. Graceful unsubscribe on shutdown.
  - Files: connection.rs, connection_pool.rs
  - Tests: test_graceful_shutdown_sets_flag_and_notifies, test_graceful_shutdown_sends_disconnect_request, test_graceful_shutdown_timeout_does_not_block, test_pool_graceful_shutdown_signals_all_connections, test_pool_graceful_shutdown_skips_dead_connections_safely

- [x] Dhan ticket lockdown — Tickets #5519522 and #5525125 enforced mechanically.
  - Files: dhan_locked_facts.rs, dhan_api_coverage.rs, live-market-feed.md, full-market-depth.md, pre-push-gate.sh, banned-pattern-scanner.sh
  - Tests: locked_ticket_5519522_twohundreddepth_path, locked_ticket_5519522_depth_hosts_are_separate, locked_ticket_5519522_depth_200_uses_atm_strike_selector, locked_ticket_5525125_idx_i_prev_close_from_code_6, locked_ticket_5525125_nse_eq_fno_prev_close_from_quote_full, locked_rule_file_cites_ticket_5519522, locked_rule_file_cites_ticket_5525125, locked_no_root_path_for_200_depth_in_source

- [x] A4. Pool-level circuit breaker watchdog.
  - Files: pool_watchdog.rs, connection_pool.rs
  - Tests: test_watchdog_starts_healthy, test_watchdog_all_connected_stays_healthy, test_watchdog_one_alive_is_healthy, test_watchdog_detects_all_reconnecting_as_degrading, test_watchdog_degrades_at_60s, test_watchdog_halts_at_300s, test_watchdog_recovery_resets_state, test_watchdog_fires_degraded_alert_exactly_once_per_cycle, test_watchdog_disconnected_counts_as_down, test_watchdog_connecting_counts_as_live, test_watchdog_empty_pool_is_degrading, test_pool_poll_watchdog_initial_state_is_degrading, test_pool_poll_watchdog_stable_across_polls

- [x] A3. Live intraday backfill worker.
  - Files: backfill.rs
  - Tests: test_gap_request_gap_secs_normal, test_gap_request_gap_secs_out_of_order_saturates, test_backfill_empty_candles_produces_empty_ticks, test_backfill_timestamp_adds_ist_offset, test_backfill_close_becomes_last_traded_price, test_backfill_multiple_candles_preserves_order, test_backfill_greeks_are_nan, test_backfill_volume_saturates_on_overflow, test_backfill_negative_volume_clamped_to_zero, test_backfill_worker_happy_path, test_backfill_worker_handles_empty_fetch, test_backfill_worker_handles_fetch_error, test_backfill_worker_aborts_on_tick_pipeline_closed

- [x] E1. Prometheus metrics catalog.
  - Files: metrics_catalog.rs
  - Tests: metrics_catalog_every_required_metric_is_emitted, metrics_catalog_no_duplicate_names, metrics_catalog_names_follow_prometheus_conventions

- [x] E2. Grafana alert rules + wiring test.
  - Files: alerts.yml, grafana_alerts_wiring.rs
  - Tests: grafana_alerts_every_required_uid_is_provisioned, grafana_alerts_every_rule_has_severity_label

- [x] B2. Disk-full chaos test.
  - Files: chaos_disk_full.rs
  - Tests: chaos_disk_full_triggers_dlq, chaos_disk_full_recovery_after_permissions_restored

- [x] B3. SIGKILL mid-batch chaos test.
  - Files: chaos_sigkill_replay.rs
  - Tests: chaos_sigkill_spill_replay_zero_loss, chaos_sigkill_recovery_idempotent_on_empty_dir

- [x] C1. Dhan doc + SDK verification report.
  - Files: verification-2026-04-13.md
  - Tests: locked_rule_file_cites_ticket_5519522, locked_rule_file_cites_ticket_5525125

## Session 2 totals

- 10 plan items complete
- ~50 new tests, all green
- 10 commits (this is the final one)
