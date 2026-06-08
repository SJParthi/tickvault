# Implementation Plan: NTM constituent dangling threshold 0.5% → 2% + log stragglers

**Status:** VERIFIED
**Date:** 2026-06-08
**Approved by:** Parthiban — AskUserQuestion 2026-06-08 "Raise threshold to 2% + skip stragglers (Recommended)".
**Crate(s) touched:** `tickvault-core` (+ rule docs).

## Context

Live AWS log (`/opt/tickvault/data/logs/errors.jsonl.2026-06-08`) root-caused the missing NTM
universe (244 subscribed instead of ~1000):

```json
{"code":"NTM-CONSTITUENCY-01","reason":"dangling",
 "err":"constituent resolve dangling fraction 5/748 = 0.0067 exceeds 0.005"}
```

The niftyindices download SUCCEEDED (748 constituents). The ISIN→Dhan-master join left **5 of 748
(0.67%) unresolved**, which exceeds the §31.1(4) `DANGLING_REJECT_FRACTION = 0.005` (0.5%) gate, so
`resolve_constituents` returns `Err(TooManyDangling)` → `resolve_ntm_rows` degrades to EMPTY → the
universe falls back to indices + F&O underlyings (~244). The 0.5% gate allows only ≤3 of 748 — a
~750-stock membership list naturally has a handful of unmatchable names (new listings, SME, ISIN
edge cases), so 5 stragglers nuking 743 good stocks is an all-or-nothing trap.

## Design

1. **`crates/core/src/instrument/constituent_resolver.rs`** — raise the NTM-only
   `DANGLING_REJECT_FRACTION` from `0.005` (0.5%) to `0.02` (2%). 0.67% < 2% → resolve now returns
   `Ok` with `resolved = 743`, `unresolved_symbols = [5 names]` (the existing under-threshold path
   already DROPS the unresolved and keeps the resolved — raising the threshold IS the "skip
   stragglers" behaviour). Update the module + constant + error docstrings to cite the 2% NTM value
   and that it is DISTINCT from the Dhan-master F&O dangling guard (`fno_underlying_extractor.rs`,
   unchanged at the §3 reject level).
2. **`crates/core/src/instrument/daily_universe_orchestrator.rs`** — in `resolve_ntm_rows`, the
   success-path `info!` already logs `ntm_unresolved` (count); extend it to also log the actual
   dropped symbol names (truncated to the first 20, joined) so every boot SHOWS which constituents
   were skipped — operator visibility per audit-findings Rule 11 (no silent drops).
3. **Rule docs** — `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §31.1(4) +
   `.claude/rules/project/ntm-constituency-error-codes.md`: record that the NTM constituent reject
   threshold is 2% (operator lock 2026-06-08), separate from the 0.5% Dhan-master F&O guard.

This is the operator-approved fix; it only loosens the NTM membership-list join tolerance — it does
NOT touch the Dhan-master fail-closed path (which stays the order-critical guard) and does NOT touch
the 2-WebSocket lock or the subscription dispatch.

## Edge Cases

- 0/0 constituents (niftyindices fully down): existing `if total > 0` guard untouched → no div-by-0;
  empty map → `resolve_ntm_rows` returns empty → core universe (NTM-CONSTITUENCY-01), unchanged.
- Exactly 2.0% dangling: code uses `fraction > FRACTION` (strict) → 2.0% does NOT reject (accepted).
- > 2% dangling (e.g. niftyindices serves a corrupt/half list): still `Err(TooManyDangling)` →
  degrade to core universe + NTM-CONSTITUENCY-01 (fail-closed preserved at the higher bar).
- Stragglers change day-to-day: the dropped-names log lists whatever the current set is each boot.

## Failure Modes

- Threshold too loose hides a real niftyindices corruption: mitigated — 2% still fail-closes a
  materially broken list; only a handful of genuine stragglers pass; dropped names are logged so a
  rising straggler count is visible.
- No new panic path (pure function, no unwrap/expect added).

## Test Plan

- `crates/core/src/instrument/constituent_resolver.rs` unit tests (run with
  `cargo test -p tickvault-core --features daily_universe_fetcher`):
  - UPDATE `fail_closed_above_half_percent_dangling` → `fail_closed_above_two_percent_dangling`
    (3/100 = 3% > 2% → reject).
  - KEEP `tolerates_below_threshold_dangling` (1/300 = 0.33% < 2% → OK).
  - ADD `tolerates_ntm_straggler_fraction` — 5 dangling / 748 total = 0.67% < 2% → `Ok`,
    `resolved.len() == 743`, `unresolved_symbols.len() == 5` (the live scenario).
  - ADD `test_dangling_reject_fraction_is_two_percent` pinning the constant = 0.02.
- `daily_universe_orchestrator.rs` existing tests still pass (logging-only change to the Ok path).

## Rollback

Single-constant revert: set `DANGLING_REJECT_FRACTION` back to `0.005` and revert the two test
edits + the log line. No data migration, no schema change, no wire-format change — pure in-process
boot-path logic. `git revert <sha>` is clean.

## Observability

- The boot `info!` in `resolve_ntm_rows` now carries `ntm_resolved`, `ntm_unresolved`, and the
  dropped symbol NAMES — so CloudWatch/`errors.jsonl`/app log show exactly which constituents were
  skipped each morning. `NTM-CONSTITUENCY-01` still fires (Critical) only when the (now 2%) gate is
  breached — i.e. a genuinely broken list, not a few stragglers.

## Plan Items

- [x] Raise `DANGLING_REJECT_FRACTION` 0.005 → 0.02 + docstrings
  - Files: crates/core/src/instrument/constituent_resolver.rs
  - Tests: test_dangling_reject_fraction_is_two_percent, fail_closed_above_two_percent_dangling, tolerates_ntm_straggler_fraction
- [x] Log dropped straggler names on the NTM resolve success path
  - Files: crates/core/src/instrument/daily_universe_orchestrator.rs
  - Tests: existing daily_universe_orchestrator tests (logging-only)
- [x] Document the 2% NTM threshold (distinct from 0.5% Dhan-master guard)
  - Files: .claude/rules/project/daily-universe-scope-expansion-2026-05-27.md, .claude/rules/project/ntm-constituency-error-codes.md
  - Tests: n/a (docs)
