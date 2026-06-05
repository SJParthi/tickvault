# Implementation Plan: Fix flaky spill-recovery count test (storage)

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban (standing directive: keep CI green, fix flakes) — follow-up to #1029, whose new trading tests shifted nextest ordering and exposed this latent flake.
**Crate(s) touched:** `storage`

## Context (verified from the CI log, no illusion)

CI "Test (storage)" failed on #1029:
`tick_persistence::tests::test_recover_stale_spill_file_on_startup` asserted it
recovered exactly **50** ticks but got **60**. Root cause: `recover_stale_spill_files()`
scans the WHOLE shared `data/spill/` dir for every `ticks-*.bin`. A leftover
`ticks-*.bin` from another test in the same nextest run (the spill tests share
the real dir under `spill_dir_test_lock`, but a count test never pre-cleans)
inflated the count by 10 → 60. It passed locally (different ordering) and on the
#1029 re-run (auto-merged before this fix), so the flake is STILL latent in main
and will randomly fail future CI.

## Plan Items

- [x] Item 1 — Add `purge_stale_tick_spill_files(dir)` test helper
  - Files: `crates/storage/src/tick_persistence.rs`
  - Tests: existing `test_recover_*` suite (8) now deterministic
- [x] Item 2 — Pre-clean stale `ticks-*.bin` in the two exact-count recovery tests
  - Files: `crates/storage/src/tick_persistence.rs`
  - Tests: `test_recover_stale_spill_file_on_startup`, `test_recover_skips_current_active_spill`

## Design

The exact-count recovery tests call `purge_stale_tick_spill_files(real_spill_dir)`
right after `create_dir_all` (under the existing `spill_dir_test_lock`), removing
any pre-existing `ticks-*.bin` so they begin from a known-empty dir. The helper is
idempotent and ignores a missing dir / per-file errors.

## Edge Cases

- Missing spill dir → `read_dir` errors → helper no-ops (safe).
- Non-`ticks-*.bin` files in the dir → untouched (only the matched pattern removed).
- Concurrent spill tests → all serialized by `spill_dir_test_lock`, so the
  pre-clean + write + recover sequence is atomic w.r.t. other spill tests.

## Failure Modes

- Test-only change; zero production code touched. Worst case a future test adds a
  spill file under a different name — out of scope (the pattern matches the
  production `ticks-*.bin` naming).

## Test Plan

- `cargo test -p tickvault-storage --lib test_recover` — 8 green (deterministic).
- `cargo clippy -p tickvault-storage -- -D warnings -W clippy::perf` clean.
- design-first wall (impl crate `storage` + this APPROVED plan).

## Rollback

Revert the helper + the two call sites. No production behaviour change.

## Observability

None — test isolation only. CI "Test (storage)" stops flaking.

## Per-Item Guarantee Matrix (cross-reference)

Bound by the 15-row 100% guarantee matrix + 7-row resilience matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`.

**Honest envelope:** "100% inside the tested envelope, with ratcheted regression
coverage" = the 8 deterministic `test_recover_*` tests. This is a test-only flake
fix making a recovery-COUNT assertion deterministic regardless of nextest parallel
ordering. No production logic change; the zero-tick-loss spill→recover behaviour is
unchanged.
