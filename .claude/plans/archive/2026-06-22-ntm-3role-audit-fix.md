# Implementation Plan: PR-F — fix NTM 3-role boot-panic in instrument_fetch_audit

**Status:** APPROVED
**Date:** 2026-06-22
**Approved by:** Parthiban (live boot panic on Mac dev — `instr_fetch_audit_writer.rs:146 assertion left==right failed: universe_size must equal index_count + underlying_count (only 2 roles exist), left: 775, right: 244`)

## Design

The §31 NTM expansion added a THIRD `InstrumentRole::IndexConstituent` (the ~531
NIFTY-Total-Market constituent stocks) to the daily universe. The
`instrument_fetch_audit` terminal-success builder still carried a TWO-role
invariant: `debug_assert_eq!(universe_size, index_count + underlying_count)`.
On a real NTM boot the universe is ~775 SIDs while `index + underlying` is ~244,
so the debug assertion fires and panics boot in dev/debug builds.

Fix: thread the ROLE-partition count `count_by_role(InstrumentRole::IndexConstituent)`
(mutually-exclusive — NOT the overlapping `is_index_constituent` flag count) from
the boot callsite into the audit-row builder, and assert the correct 3-role
partition sum `universe_size == index + underlying + index_constituent`.

Touched crate: `app` (`crates/app/src/instr_fetch_audit_writer.rs`,
`crates/app/src/daily_universe_boot.rs`).

## Edge Cases

- Flag count vs role count: `index_constituent_count()` (flag) includes the
  `FnoUnderlying ∩ IndexConstituent` overlap and would break the partition sum;
  the builder takes the disjoint `count_by_role(IndexConstituent)` instead. The
  boot log keeps emitting the flag count for observability (forward-correct), the
  audit row uses the role-partition count for the invariant.
- Pre-NTM small test universes (constituent=0): 2-term and 3-term sums coincide,
  so existing tests stay valid; a new 3-role test exercises the non-zero case.
- `Exhausted` outcome: still a no-op `Ok` — no row, no assertion.

## Failure Modes

- `debug_assert_eq!` is debug-only (compiles out in release) — this is a
  defensive invariant, never a release-path panic. The fix corrects the invariant
  so dev/debug boots no longer false-panic on a valid NTM universe.
- `i64::try_from(...).unwrap_or(i64::MAX)` saturates rather than panicking if a
  count somehow exceeds i64 (impossible at the [100,1200] envelope, defensive).

## Test Plan

- `test_build_terminal_success_row_populates_counts_and_attempt` updated to a
  real 3-role case (30 + 218 + 524 = 772).
- New `test_build_terminal_success_row_three_role_partition_holds` asserts the
  partition sum produces a row.
- `test_build_terminal_success_row_none_for_exhausted`,
  `test_build_terminal_success_row_propagates_dry_run`,
  `test_record_terminal_success_*` updated to the new arity.
- `cargo build -p tickvault-app` + `cargo test -p tickvault-app --lib
  instr_fetch_audit` green (12/12).
- Adversarial parallel agent hunt: confirmed this was the SOLE stale 2-role
  assertion; no other `assert`/`panic`/match-arm risk remains.

## Rollback

Additive parameter + corrected assertion. Reverting restores the (buggy) 2-role
assertion; no data-model change, no schema change, no wire-format change (the
audit row columns are unchanged — `index_constituent_count` is an input to the
invariant, not a new persisted column).

## Observability

No new telemetry needed — the boot `info!` line already emits `universe_size`,
`index_count`, `fno_underlying_count`, `index_constituent_count`. The audit row
schema is unchanged. The corrected invariant simply stops a false dev-boot panic.

## Plan Items

- [x] PR-F — thread `count_by_role(IndexConstituent)` into the audit-row builder;
  assert the 3-role partition sum; update tests. Files:
  `crates/app/src/instr_fetch_audit_writer.rs`,
  `crates/app/src/daily_universe_boot.rs`. Tests:
  `test_build_terminal_success_row_three_role_partition_holds`,
  `test_build_terminal_success_row_populates_counts_and_attempt`.

## Guarantee matrix

This item carries the operator-mandated 15-row "100% everything" matrix + the
7-row resilience demand matrix by cross-reference to the canonical
`.claude/rules/project/per-wave-guarantee-matrix.md`. Item-specific proof: 100%
testing via 12/12 `instr_fetch_audit` tests green; 100% code checks via
banned-pattern + pub-fn-test + wiring + plan-gate; 100% bug-fixing + code review
via the adversarial parallel agent hunt (confirmed sole occurrence, no remaining
2-role risk); 100% performance via debug-only assertion (zero release overhead,
O(1)); 100% scenarios via the 3-role partition test covering the real ~775-SID
NTM universe that triggered the live panic. Resilience (7-row): no new tick-drop
path (audit-row builder is cold-path, boot-time only); O(1) per-call;
uniqueness/dedup unchanged (`instrument_fetch_audit` DEDUP keys untouched).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Real NTM boot (~775 SIDs, debug build) | No panic; 3-role sum holds; clean Success audit row |
| 2 | Small pre-NTM test universe (constituent=0) | 2-term == 3-term; unchanged behaviour |
| 3 | Release build | Assertion compiled out; identical to before |
| 4 | Exhausted outcome | No-op Ok, no row, no assertion |
