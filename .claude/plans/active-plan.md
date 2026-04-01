# Implementation Plan: QuestDB Resilience Hardening + Storage Proptest + Monitoring

**Status:** DRAFT
**Date:** 2026-04-01
**Approved by:** pending

## Context

Deep analysis revealed 6 real gaps in QuestDB resilience and storage testing.
The 3-tier zero-loss architecture (ring buffer → disk spill → recovery) is solid,
but needs: property testing, periodic fsync, threshold alerts, better gap detection,
and updated docstrings.

## Plan Items

- [ ] P1: Add `proptest` dev-dependency to storage crate + property-based tests
  - Files: crates/storage/Cargo.toml, crates/storage/tests/proptest_storage.rs
  - Tests: proptest_serialize_deserialize_roundtrip, proptest_buffer_count_invariant,
           proptest_f32_to_f64_clean_never_panics, proptest_spill_record_size_constant,
           proptest_ring_buffer_never_exceeds_capacity

- [ ] P2: Add periodic disk spill flush (fsync on heartbeat) for durability
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_periodic_spill_flush_syncs_writer, test_flush_spill_no_op_when_no_writer

- [ ] P3: Add ring buffer utilization threshold alerts (50%, 80%, 95% → metrics + log)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_buffer_threshold_50_percent_alert, test_buffer_threshold_80_percent_alert,
           test_buffer_threshold_95_percent_alert, test_no_duplicate_threshold_alerts

- [ ] P4: Improve post-recovery gap detection from 1-minute to 10-second granularity
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_gap_check_query_uses_10s_sample, test_gap_check_detects_sub_minute_gaps

- [ ] P5: Fix outdated docstring (line 13-16 says "no buffering" but we DO buffer)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: (docstring only, no test needed)

- [ ] P6: Add stress test for prolonged outage scenario (full ring + disk spill + recovery)
  - Files: crates/storage/tests/tick_resilience.rs
  - Tests: test_prolonged_outage_ring_plus_spill_zero_loss,
           test_concurrent_append_during_threshold_alerts

- [ ] P7: Archive previous plan + run full workspace build + clippy + test
  - Files: (none — verification only)
  - Tests: cargo test --workspace

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
