# Implementation Plan: Zero Tick Loss Session 3 — Extreme Assurance Hardening

**Status:** VERIFIED
**Date:** 2026-04-13
**Approved by:** Parthiban ("cover even the left steps")
**Branch:** `claude/websocket-zero-tick-loss-nUAqy`
**Previous session archived:** `archive/2026-04-13-zero-tick-loss-session-2.md`

## Goal

Push the zero-tick-loss posture from "defence-in-depth" to "observable guarantee". Wire the deferred A3 backfill worker, add a QuestDB health poller so disconnects are survivable AND visible, lock down DEDUP uniqueness with property tests, cap the backfill synth at bounded allocation via DHAT, add cargo audit + cargo deny to pre-push, and lock the 100% coverage policy itself so it can't be silently weakened.

## Plan items

- [x] S3-1. QuestDB health poller state machine.
  - Files: questdb_health.rs
  - Tests: test_poller_starts_healthy, test_poller_healthy_stays_healthy, test_poller_disconnect_transition_increments_counter, test_poller_fires_degraded_at_30s, test_poller_fires_halt_at_300s, test_poller_recovery_increments_reconnect_counter, test_poller_multiple_outages_each_counted, test_poller_degraded_fires_exactly_once_per_outage, test_poller_recovery_resets_alert_flag

- [x] S3-5. Remove dead PREVIOUS_CLOSE constants.
  - Files: tick_persistence.rs
  - Tests: locked_ticket_5525125_idx_i_prev_close_from_code_6

- [x] S3-2. Wire A3 backfill worker into main.rs.
  - Files: main.rs
  - Tests: test_backfill_worker_happy_path, test_backfill_worker_handles_empty_fetch

- [x] S3-3. Uniqueness property test for DEDUP.
  - Files: dedup_uniqueness_proptest.rs
  - Tests: prop_dedup_tuple_is_deterministic, prop_non_key_fields_do_not_affect_tuple, prop_different_security_id_different_tuple, prop_different_segment_different_tuple, prop_different_timestamp_different_tuple, dedup_key_string_contains_security_id_and_segment, dedup_regression_nse_bse_1333_collision, dedup_backfill_replay_idempotent

- [x] S3-4. DHAT bounded allocation test for A3 synth.
  - Files: dhat_backfill_synth.rs
  - Tests: dhat_backfill_synth_bounded_allocations

- [x] S3-6. Pre-push Gate 10 cargo audit + cargo deny + deny.toml lockdown.
  - Files: pre-push-gate.sh, deny_config_wiring.rs
  - Tests: deny_config_exists_and_has_required_sections, deny_config_targets_include_linux_and_mac, deny_config_not_empty

- [x] S3-7. Coverage threshold lockdown at 100%.
  - Files: coverage_threshold_lockdown.rs
  - Tests: coverage_lockdown_file_exists, coverage_lockdown_default_is_100, coverage_lockdown_every_crate_is_100, coverage_lockdown_required_crates_are_listed

## Session 3 totals

- 7 plan items complete
- ~35 new tests, all green
- 13 new metrics catalogued (4 QuestDB health + 7 backfill + 2 A4/A5 already catalogued)
- 1 new Grafana alert (tv-questdb-disconnected)
- 1 new pre-push gate (Gate 10 cargo audit + deny)
