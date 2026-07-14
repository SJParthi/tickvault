# Implementation Plan: #1481 spill-test cross-process race fix + 1481(b) APPROVED sweep

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — default-authorized the 1481(b) APPROVED sweep 2026-07-14 (relayed via coordinator)

> Scope: the `storage` crate, single file `crates/storage/src/tick_persistence.rs`.
> Zero runtime behavior change — production code gains only `// APPROVED:` scanner
> annotations + one comment reword; the only executable-code edits are inside two
> `#[test]` functions. Landed via PR #1546.

## Plan Items

- [x] Per-process spill-dir isolation for the 2 racing spill-recovery tests
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_recover_stale_spill_file_on_startup, test_recover_skips_current_active_spill

- [x] 32-hit `// APPROVED:` false-positive annotation sweep (banned-pattern 16 + dedup/O(1) 16) + 1 comment reword
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: covered by the existing storage suite (1168 passed / 0 failed) — annotations are comments only; scanner cleanliness verified by banned-pattern-scanner.sh + dedup-latency-scanner.sh exit 0

## Design

`tick_persistence::tests::test_recover_stale_spill_file_on_startup` and
`test_recover_skips_current_active_spill` operated on the SHARED default spill dir
and assert exact recovered-tick counts. Under nextest each test runs in its own OS
process, so the in-process `spill_dir_test_lock()` mutex serializes nothing across
processes — two concurrent processes create/steal each other's `ticks-*.bin` files
(the observed 70-vs-50 / 60-vs-10 CI failure signatures on `Test (storage)`).

Fix: each test uses a unique per-process temp spill dir
(`std::env::temp_dir().join(format!("tv-recover-…-{pid}"))`) wired through the
PRE-EXISTING `set_spill_dir_for_test()` knob (the F1 house pattern already used by
`tv-p2b-ringfull-{pid}` in the same file). The lock guard + the
`purge_stale_tick_spill_files` helper are KEPT (defensive on pid reuse), and each
test `remove_dir_all`-cleans its unique dir on exit.

Companion prerequisite (1481(b)): the local banned-pattern + dedup/O(1) scanners
scan the WHOLE staged file, so any edit trips 32 pre-existing false positives
(boot-time DDL, outage-only lazy spill/DLQ opens, post-reconnect recovery drains,
`#[cfg(test)]` helpers leaked by the dedup scanner's awk). Each hit was individually
verified false-positive and annotated with a standalone `// APPROVED: <reason>`
line (each exempts exactly the one following line); one comment containing the
thread-sleep literal was reworded. No genuine violation found; nothing per-tick
was annotated.

## Edge Cases

- Pid reuse re-colliding on a leftover temp dir: the retained purge helper clears
  pre-existing spill files inside the test dir before the test body runs.
- Leftover dirs from a killed test process: each dir is per-pid under the OS temp
  dir and is `remove_dir_all`-cleaned on the next same-pid run; orphans are inert
  (never read by production code, which uses `data/spill`).
- Pre-existing spill files in the test dir: purged at test start (count assertions
  stay exact).
- Same-process serial re-run: the in-process lock guard still serializes; the
  purge + unique dir make it idempotent.
- Annotation adjacency: every `// APPROVED:` line was placed immediately above its
  exact target line and traced through both scanners' one-line `skip_next` awk —
  no annotation shadows a different or additional line.

## Failure Modes

- Annotation shadowing risk (an annotation silently exempting a future NEW
  violation): ruled out — both scanners exempt exactly the ONE following line;
  adjacency was traced per annotation (security-reviewer pass, no findings above
  LOW-informational).
- Genuine hot-path violation being papered over: ZERO found — every annotated line
  executes only on boot/outage/recovery/test paths; the per-tick happy path
  (`append_tick_with_seq` → buffer build → batch dispatch) contains none of the
  annotated lines (hot-path-reviewer pass: NO FINDINGS).
- The race resurfacing via a third test sharing the default spill dir: the two
  count-asserting tests are now isolated; any future shared-dir count assertion
  would flake CI loudly (the same #1481 signature), not silently.
- Temp-dir cleanup failure: non-fatal — orphan dirs are inert and per-pid.

## Test Plan

- Full storage suite on the landed tree: `cargo test -p tickvault-storage` —
  1168 passed / 0 failed (1148/0 on the pre-rebase tree).
- Two-process concurrency repro: 5/5 rounds green with both tests run
  simultaneously in separate OS processes, each run VERIFIED to execute exactly
  its 1 target test (`1 passed; 0 failed` asserted per log).
- Scanners on the file: banned-pattern-scanner.sh exit 0 (was 16 hits),
  dedup-latency-scanner.sh exit 0 (was 16 hits), data-integrity-guard.sh CLEAN.
- `cargo fmt --check` clean; clippy — no warnings attributable to the diff.
- Adversarial review: hot-path-reviewer NO FINDINGS; security-reviewer no
  CRITICAL/HIGH/MEDIUM (2 LOW informational, accepted).

## Rollback

`git revert` of the single code commit (`4fd49c63`, carried through the branch
merges) restores the prior file byte-for-byte. Test-only + comments-only change —
no runtime behavior, no schema, no config, no migration; rollback risk is nil
(the CI flake would simply return).

## Observability

N/A runtime — this change adds no production code path, metric, log line, or
alert (nothing to observe at runtime by design). The observable is CI itself:
the `Test (storage)` job's flake rate on the two spill-recovery tests, which this
fix drives to zero (any regression re-fires the exact #1481 count-mismatch
failure signature loudly in CI). Scanner cleanliness on the file is continuously
enforced by the pre-commit/pre-push banned-pattern + dedup/O(1) + data-integrity
guards, which remain armed for any future NEW violation.
