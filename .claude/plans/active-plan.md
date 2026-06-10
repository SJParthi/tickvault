# Implementation Plan: P2b — tick_persistence error-arm coverage (ring→spill→DLQ)

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban (operator, 2026-06-10: "once it gets merged go ahead with the plan always" — P2 approved as "write the plain missing unit tests (error branches)"; P2b is the storage slice, following merged P2a #1081)
**Authority:** `.claude/plans/research/coverage-gaps.md` §5 rank 14 + §6 P2 (merged #1076).

**Per-item guarantee matrix:** cross-references `.claude/rules/project/per-wave-guarantee-matrix.md`. Deltas: test-only change inside the existing `#[cfg(test)] mod tests` of `crates/storage/src/tick_persistence.rs` — zero production-code lines changed, no hot-path impact (tests are compiled out of prod via cfg), no new pub fn, no new dep.

## Plan Items

- [x] Cover the TICK-SEQ-01 `_seq` disconnected/error arms in tick_persistence
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_append_tick_enriched_with_seq_disconnected_buffers_and_preserves_seq,
    test_flush_buffer_direct_disconnected_returns_error,
    test_buffer_tick_seq_ring_full_spills_with_preserved_seq,
    test_spill_open_failure_double_fault_counts_dropped,
    test_write_to_dlq_success_writes_ndjson_line,
    test_drain_disk_spill_disconnected_preserves_spill_file
  - The uncovered arms (report Appendix A: `append_tick_enriched_with_seq`
    disconnected rescue, `flush_buffer_direct` reconnect-fail, `buffer_tick_seq`
    ring-full spill, `spill_tick_to_disk_seq` open-fail → DLQ fallback,
    `write_to_dlq` happy + open-fail paths, `drain_disk_spill` flush-fail
    preserve-file branch, `rescue_in_flight` via failed force_flush) are
    exercised via `new_disconnected()` + per-test temp spill dirs — the exact
    pattern the file's existing tests already use.

## Design

Inline unit tests appended to the existing `mod tests` (they need access to
private methods `buffer_tick_seq`, `spill_tick_to_disk`, `write_to_dlq`,
`drain_disk_spill`, `build_tick_row_seq`, `deserialize_tick_seq` — same access
level the module's current tests use). Each test gets a unique
`std::env::temp_dir()` subdirectory via `set_spill_dir_for_test` (the file's
established F1 idiom) so parallel test runs never collide. The double-fault
test points the spill dir at an uncreatable `/proc/...` path so BOTH the spill
open AND the DLQ open fail deterministically — covering the
"tick PERMANENTLY LOST" accounting arm without touching real disks.

## Edge Cases

- capture_seq preservation through ring AND disk spill (deserialize the
  spilled record and assert the exact seq round-trips — the TICK-SEQ-01
  replay-stability contract).
- Ring exactly at `TICK_BUFFER_CAPACITY`: the +1th tick spills, the ring stays
  at capacity (no drop, no overwrite).
- DLQ NDJSON line carries the literal `reason` + `security_id` (audit
  recoverability).
- `drain_disk_spill` with no QuestDB: flush fails → spill file is PRESERVED
  (not deleted) and `spill_path` restored for the next recovery attempt.
- Tests never touch the real `data/spill/` (per-test temp dirs only).

## Failure Modes

- If a future refactor silently drops the ring-full spill (ticks lost), the
  spill-count + file-existence assertions fail the build.
- If the DLQ accounting regresses (dropped counter not incremented on double
  fault), the double-fault test fails.
- If capture_seq stops round-tripping through the spill record, the
  preserved-seq assertion fails (TICK-SEQ-01 regression).
- Flaky-risk: none — no network, no timing assumptions; the disconnected
  config uses port 1 (reserved, guaranteed connect failure), same as the
  module's existing tests.

## Test Plan

- `cargo test -p tickvault-storage --lib` (inline tests) +
  `cargo test -p tickvault-storage` scoped suite — all green.
- `cargo fmt --check`; strict clippy on the touched crate's lib tests is NOT
  CI-gated (ci.yml lints libs/bins only) but the new tests are kept clippy-clean
  under `-D warnings` for the test target anyway.
- Pre-commit invariant gates (dedup proptest + bible lockdown) on warm cache.

## Rollback

`git revert` of the single squash commit removes only test code inside
`#[cfg(test)]`. Zero production-behavior surface.

## Observability

- N/A at runtime (cfg(test)-only). CI-side effect: storage-crate line coverage
  rises (~+100 lines ≈ +0.8pp); floors ratchet up only on a re-measured
  baseline per `quality/crate-coverage-thresholds.toml` (not in this PR).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Enriched+seq append while QuestDB down | Ok(()) + tick in ring with its seq |
| 2 | Direct buffer flush while down | Err (reconnect fails), no panic |
| 3 | Ring full, one more tick | spilled_total=1, spill file on disk, seq preserved byte-exact |
| 4 | Spill dir uncreatable (double fault) | dropped_total=1, no panic, loud arm executed |
| 5 | DLQ write | dlq_total=1, NDJSON line has reason + security_id |
| 6 | Spill drain while still down | file preserved, spill_path restored |
