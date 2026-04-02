# Implementation Plan: QuestDB Resilience Hardening + Storage Proptest + Monitoring

**Status:** VERIFIED
**Date:** 2026-04-01
**Approved by:** Parthiban ("Go ahead dude")

## Context

Deep analysis revealed 6 real gaps in QuestDB resilience and storage testing.
The 3-tier zero-loss architecture (ring buffer → disk spill → recovery) is solid,
but needs: property testing, periodic fsync, threshold alerts, better gap detection,
and updated docstrings.

## Plan Items

- [x] P1: Add `proptest` dev-dependency to storage crate + property-based tests
  - Files: crates/storage/Cargo.toml, crates/storage/tests/proptest_storage.rs
  - Tests: proptest_buffer_count_invariant, proptest_ring_buffer_never_exceeds_capacity,
           proptest_append_tick_always_ok_disconnected, proptest_zero_drops_within_capacity,
           proptest_rapid_append_flush_no_corruption

- [x] P2: Add periodic disk spill flush (fsync on heartbeat) for durability
  - Files: crates/storage/src/tick_persistence.rs (flush_spill_if_needed, flush_if_needed)
  - Tests: test_periodic_spill_flush_syncs_writer, test_flush_spill_no_op_when_interval_not_elapsed,
           test_spill_flush_interval_constant_is_30s

- [x] P3: Add ring buffer utilization threshold alerts (50%, 80%, 95% → metrics + log)
  - Files: crates/storage/src/tick_persistence.rs (check_buffer_thresholds, buffer_tick)
  - Tests: test_buffer_threshold_50_percent_alert, test_buffer_threshold_80_percent_alert,
           test_buffer_threshold_95_percent_alert, test_no_duplicate_threshold_alerts,
           test_threshold_alerts_reset_below_25_percent, test_buffer_threshold_constants_are_ordered

- [x] P4: Improve post-recovery gap detection from 1-minute to 10-second granularity
  - Files: crates/storage/src/tick_persistence.rs (check_tick_gaps_after_recovery)
  - Tests: test_gap_check_query_uses_10s_sample, test_gap_check_10s_detects_sub_minute_gaps

- [x] P5: Fix outdated docstring (line 13-16 says "no buffering" but we DO buffer)
  - Files: crates/storage/src/tick_persistence.rs (module docstring)
  - Tests: (docstring only, no test needed)

- [x] P6: Add stress test for prolonged outage scenario (full ring + disk spill + recovery)
  - Files: crates/storage/tests/tick_resilience.rs
  - Tests: test_prolonged_outage_ring_plus_spill_zero_loss,
           test_flush_if_needed_triggers_spill_flush_during_outage,
           test_threshold_alerts_during_outage

- [x] P7: Archive previous plan + run full workspace build + clippy + test
  - Files: crates/app/src/main.rs (clippy fix)
  - Tests: cargo fmt --check + cargo clippy + cargo test --workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB down at boot | Writer starts disconnected, all ticks buffered, zero loss |
| 2 | QuestDB crashes mid-session | Flush fails, sender=None, ticks rescued to ring buffer |
| 3 | Ring buffer fills (300K ticks) | Overflow spills to disk file, zero loss |
| 4 | Disk spill file grows | Warning every 10K ticks, threshold alerts at 50/80/95% |
| 5 | QuestDB recovers | Ring buffer drains first, then disk spill, then resume |
| 6 | Power loss during spill | Periodic fsync limits data loss to ~1 flush interval |
| 7 | Stale spill files from crash | Recovered on next startup before normal operation |
| 8 | Random tick data (proptest) | Serialize/deserialize roundtrip always preserves data |
