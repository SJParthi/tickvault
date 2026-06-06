# Implementation Plan: NTM Sub-PR #2 — raise universe cap 400 → 1200

**Status:** APPROVED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion 2026-06-06 chose "Raise to 1200";
recorded in `daily-universe-scope-expansion-2026-05-27.md` §31.
**Crate(s) touched:** `common` (constant + error_code doc), `core`
(daily_universe boundary tests + docs), `storage` (2 scope guards), `trading`
(aggregator scale guard), `app` (daily_universe_boot doc comment), + the rule
files. No new runtime behavior — only the envelope bound widens.

## Context

Master roadmap lives in §31 of the daily-universe rule. Sub-PR #1 (docs
authorization) merged as #1034. This Sub-PR #2 raises the mechanical universe
envelope so the later sub-PRs (NTM constituency downloader, mapping, subscription
wiring) don't trip the boot HALT. The actual subscribed universe stays ~250 until
Sub-PR #5 — this PR only widens the *allowed* ceiling.

**Deferred (NOT in this PR):** adding NIFTY Total Market to `NSE_INDEX_ALLOWLIST`.
The allowlist requires the EXACT Dhan `SYMBOL_NAME`, which is not present in the
repo (no master CSV on disk) and must not be guessed. It lands once the symbol is
verified (operator-provided or resolved from the live master in Sub-PR #3).

## Plan Items

- [ ] Item 1 — `MAX_DAILY_UNIVERSE_SIZE` 400 → 1200 + every coupled literal
  - Files: `crates/common/src/constants.rs`, `crates/common/src/error_code.rs`,
    `crates/core/src/instrument/daily_universe.rs`,
    `crates/storage/tests/boot_step_timeout_constants_guard.rs`,
    `crates/storage/tests/daily_universe_scope_guard.rs`,
    `crates/trading/tests/aggregator_daily_universe_scale_guard.rs`,
    `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
  - Tests: `daily_universe::tests::{accepts_universe_exactly_at_max_size,
    rejects_universe_above_max_size, rejects_universe_below_min_size,
    test_max_daily_universe_size_constant}`, `boot_step_timeout_constants_guard`,
    `daily_universe_scope_guard`, `aggregator_daily_universe_scale_guard`

## Design

`MAX_DAILY_UNIVERSE_SIZE` is a pure validation bound — `build_daily_universe`
returns `UniverseSizeOutOfBounds` if `total ∉ [MIN, MAX]` (boot HALT). It is NOT a
pre-allocation: the aggregator is sized by the independent `AGGREGATOR_CAPACITY =
11_000` (already > the future ~1,000 universe) and `SEAL_BUFFER_CAPACITY = 200_000`
≥ `1200 × 21 = 25,200`. So 400 → 1200 needs **no** capacity bump and **no** RAM
change today. Every guard/test that hard-codes `400` (or `[100, 400]`) flips to
`1200` (`[100, 1200]`) in the same commit so the ratchets stay green and honest.

## Edge Cases

- `accepts_universe_exactly_at_max_size` built 400 ("exactly MAX") → rebuild to
  1200 (30 + 1 + 1169) so it still exercises the upper boundary.
- `rejects_universe_above_max_size` built 531 (was > 400) → 531 is now *accepted*;
  rebuild to 1201 (30 + 1 + 1170) so it still exercises rejection above MAX.
- `rejects_universe_below_min_size` (81 < 100) still rejects, but the reported
  `max` field in the error is now 1200 → update the matched literal.
- Rule-file guards scan for the `[100, 1200]` / `MAX_DAILY_UNIVERSE_SIZE = 1200`
  literals → update §2/§9-L4/§11 mentions in lockstep.

## Failure Modes

- Pure-constant + test change; no runtime path alters. Worst case is a missed
  `400` literal → a guard stays red and blocks the merge (fail-closed, caught by
  CI). The grep enumerated every cap-coupled `400` (excluding unrelated 86_400 /
  32_400 / 14400).

## Test Plan

- `cargo test -p tickvault-core --lib daily_universe` (4 boundary tests green).
- `cargo test -p tickvault-storage --test boot_step_timeout_constants_guard
  --test daily_universe_scope_guard`.
- `cargo test -p tickvault-trading --test aggregator_daily_universe_scale_guard`.
- `cargo build --workspace` green.

## Rollback

- All edits are a constant + tests + docs. `git revert <sha>` restores the 400
  cap. No data/runtime migration; the live universe (~250) is inside both 400 and
  1200, so neither direction changes behavior today.

## Observability

- No runtime telemetry change (the bound isn't hit at ~250 SIDs). When the
  subscribed universe actually grows (Sub-PR #5/#6), `tv_universe_size{kind}` +
  the boot HALT on > 1200 are the observability surface — specified there, not
  here. N/A for this PR.

## Per-Item Guarantee Matrix

| Demand | Sub-PR #2 |
|---|---|
| 100% testing | 4 boundary tests + 3 guards updated + green build |
| O(1)/perf | no hot-path change; cap is a boot-time bound, not pre-alloc (AGGREGATOR_CAPACITY=11k unchanged) |
| zero-loss/WS/QuestDB | unchanged — no WS, persistence, or tick path touched |
| uniqueness/dedup | unchanged — composite key + DEDUP untouched |
| honest envelope | widening the *allowed* ceiling only; real RAM at ~1,000 SIDs is still the MEASURED gate in Sub-PR #6 |
| scenarios | below-MIN / above-MAX / exactly-MAX boundary tests all re-pinned at the new cap |
