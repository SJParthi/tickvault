# Implementation Plan: Dead-Code Removal Batch 3 — crates/app + crates/api (boot/infra/health-setter/feed-state clusters)

**Status:** APPROVED
**Date:** 2026-07-18
**Approved by:** Parthiban (operator, standing dead-code removal order 2026-07-17, relayed via coordinator; LANE COMPLETE received)

Per-Item Guarantee Matrix: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` — this batch is deletion-only (zero new pub fns, zero new tables, zero hot-path involvement); the 15-row + 7-row matrices apply as N/A-with-reason per the z-plus "cosmetic refactor / deletion" carve-out, with the surviving ratchets (banned-pattern scanner, per-crate test suites, fmt, clippy) as the regression proof.

## Design

Remove verified-dead code in crates/app and crates/api per the area-3 inventory (scratchpad deadcode/area3-api-app.md), re-verified by grep at origin/main c1e5991f. Clusters: (A) crates/app/src/boot_helpers.rs dead fast-boot/watchdog-twin leftovers (spawn_heartbeat_watchdog, effective_ws_stagger, determine_boot_mode, should_fast_boot, FAST_BOOT_WINDOW_START/END, OFF_HOURS_CONNECTION_STAGGER_MS, create_log_file_writer) — WATCHDOG_INTERVAL_SECS is KEPT (live at main.rs process-global pinger). (B) crates/app/src/infra.rs dead watchdog-era cluster (check_disk_space + MIN_FREE_DISK_PERCENT, check_memory_rss, check_spill_file_size, cleanup_old_spill_files, the QuestDB-liveness block: check_questdb_liveness + QuestDbLivenessOutcome + questdb_liveness_failures + QUESTDB_LIVENESS_FAILURE_THRESHOLD, check_and_restart_containers, export_system_metrics, orphaned INFRA_WATCHDOG_INTERVAL) plus the covering tests in crates/app/tests/infra_runtime_helpers.rs (live sd_notify + container_health_counts tests stay). (C) crates/api/src/state.rs fully-orphaned pair set_questdb_reachable_for_exit_decision + questdb_reachable_for_exit_decision (+ field) and zero-caller set_boot_epoch_secs (getter + field stay — /board reads them). (D) crates/api/src/feed_state.rs dead lane-lifecycle fns (dhan_flag, groww_flag, mark_dhan_lane_running, mark_dhan_pool_present, is_dhan_pool_present, clear_live_token_manager, mark_groww_lane_running) + their own-file tests; set_dhan_lane_running/set_groww_lane_running KEPT (feed_toggle_lifecycle_guard harness). (E) vacuous guard test files crates/app/tests/ws_pool_online_truthful_emit_guard.rs + premarket_profile_halt_guard.rs, and the three zero-production-constructor NotificationEvent variants WebSocketPoolOnline, WebSocketPoolPartialAfterDeadline, PreMarketProfileCheckFailed in crates/core/src/notification/events.rs (variant decl + every match arm + in-file tests + the event_formatting_coverage.rs construct blocks).

## Edge Cases

WATCHDOG_INTERVAL_SECS gained a live production caller (main.rs process-global systemd pinger) and a guard test (systemd_boot_notify_guard.rs) — it is skipped, not deleted. set_websocket_connections is pinned by the health_counter_fix7_guard ratchet and set_order_update_connected / set_pipeline_active / set_tick_* are the only way the /health render tests exercise non-default payloads — all skipped with notes (A3-20..23; A3-22/23 additionally cleanup-lane handoff). main.rs in-file test test_boot_helper_functions_callable is trimmed (create_log_file_writer call + import removed) rather than deleted, preserving the compute_market_close_sleep smoke. Stale comments referencing deleted fns (main.rs lane comments, dhan_rest_stack.rs token-slot comment, feed_state doc comments) are reworded minimally. cadence_boot_wiring_guard bans the dhan_flag()/groww_flag() literals in cadence_boot.rs source only — deleting the fns keeps that guard green.

## Failure Modes

A missed match arm on a deleted event variant fails compile in tickvault-core — the compiler enumerates every site, so no silent partial removal is possible. A missed test import fails compile in the test target. If any deleted symbol had an unseen caller, cargo check across all six crates fails loudly before commit. The error_code cross-ref/tag guards are unaffected (NotificationEvent variants are not ErrorCodes) but are re-run explicitly to prove it. If the banned-pattern scanner or fmt/clippy flags anything, the change is fixed before push, never bypassed.

## Test Plan

cargo test -p tickvault-api, -p tickvault-app, -p tickvault-core (events.rs edits) all green; cargo check -p all six crates; cargo fmt --check; cargo clippy -p tickvault-app -p tickvault-api --no-deps; bash .claude/hooks/banned-pattern-scanner.sh; cargo test -p tickvault-common --test error_code_rule_file_crossref and --test error_code_tag_guard to prove events.rs edits break no code guards. Per deleted symbol a zero-reference grep is recorded in the PR body as evidence.

## Rollback

Single-purpose branch claude/deadcode-batch3-app-api off origin/main; revert = git revert of the squash-merge commit (pure deletions, no schema, no config, no deploy-path change, no behavior change on any live path). No data migration, no table, no alarm touched.

## Observability

No observability surface changes: no metric emit site, alarm, Telegram event with a live emitter, or log sink is removed — the three deleted NotificationEvent variants have zero production constructors (render-only dead weight), and the deleted infra probes/exporters had zero production call sites (the live QuestDB probing is storage::boot_probe + wal_suspension_watcher, untouched). The /health and /board payload fields and getters remain rendered exactly as before.

## Plan Items

- [x] A: boot_helpers.rs dead cluster removal + main.rs test trim
  - Files: crates/app/src/boot_helpers.rs, crates/app/src/main.rs
  - Tests: existing suites (deletion-only)
- [x] B: infra.rs dead cluster + infra_runtime_helpers.rs trim
  - Files: crates/app/src/infra.rs, crates/app/tests/infra_runtime_helpers.rs
  - Tests: sd_notify_sends_watchdog_and_ready_payloads_to_notify_socket, container_health_counts_never_reports_more_healthy_than_total (kept)
- [x] C: state.rs orphaned setter/getter pair + set_boot_epoch_secs
  - Files: crates/api/src/state.rs
  - Tests: existing api suite
- [x] D: feed_state.rs dead lane fns + own-file tests
  - Files: crates/api/src/feed_state.rs, crates/app/src/main.rs, crates/app/src/dhan_rest_stack.rs
  - Tests: existing api/app suites
- [x] E: vacuous guard files + 3 dead NotificationEvent variants
  - Files: crates/app/tests/ws_pool_online_truthful_emit_guard.rs (deleted), crates/app/tests/premarket_profile_halt_guard.rs (deleted), crates/core/src/notification/events.rs, crates/core/tests/event_formatting_coverage.rs
  - Tests: existing core suite
