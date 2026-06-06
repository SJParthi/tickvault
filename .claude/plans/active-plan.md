# Implementation Plan: F2 — market-open self-test asserts the live universe is complete (33 indices + NTM stocks)

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — "except brutex plan implement everything else dude okay?" (F1 + #1046
merged; implement F2 now; brutex is out of scope / different repo).
**Crate(s) touched:** `core` (`crates/core/src/instrument/market_open_self_test.rs` — 2 new
sub-checks + tests) and `app` (`crates/app/src/main.rs` — populate the 2 new inputs from the stashed
universe). **No new ErrorCode** — reuses SELFTEST-01 (pass) / SELFTEST-02 (fail), so no rule-file /
cross-ref / runbook churn.

## Context

The §31 NTM chain is merged (live subscription = 33 index values + ~748 NTM constituent stocks) and
F1 proved the build path (#1046). The remaining guard is a FOREVER self-check: every trading day at
09:16 IST the existing market-open self-test must also confirm the **live universe is complete** —
the 33 index values and the ~748 NTM constituents are actually in the subscription — so a silent
regression (niftyindices filename change, a dropped batch, an allowlist drift) pages the operator
instead of going unnoticed. This turns the daily SELFTEST-01 "Passed" ping into positive proof the
universe is whole, and routes any shortfall to the existing SELFTEST-02 Degraded page.

## Design

- Add 2 fields to `MarketOpenSelfTestInputs`: `index_values_subscribed: usize`,
  `ntm_constituents_subscribed: usize`.
- Add 2 floor consts:
  - `INDEX_VALUES_SUBSCRIBED_FLOOR = 30` — the allowlist is 33 (32 NSE incl. NIFTY TOTAL MKT + 1 BSE
    SENSEX); ≤3 legit per-day absences are tolerated (the exact missing index is already named by the
    boot `allowlist_misses` telemetry). Below 30 = the index universe collapsed.
  - `NTM_CONSTITUENTS_SUBSCRIBED_FLOOR = 600` — NTM is ~748; tolerate churn / partial; below 600 =
    the constituent layer broke or degraded.
- Add 2 sub-checks in `evaluate_self_test`, both **non-critical** (Degraded class — the feed still
  trades indices + F&O even with a partial constituent set; not added to `CRITICAL_CHECK_NAMES`):
  `index_universe_complete`, `ntm_universe_complete`.
- Bump `TOTAL_SUB_CHECKS` 6 → 8; fix stale "seven"/"six" doc wording.
- Wire in `main.rs` self-test closure: read `prev_day_ohlcv_boot::stashed_universe()` (already used at
  main.rs:3633), compute `count_by_role(InstrumentRole::Index)` + `index_constituent_count()`,
  populate the 2 new inputs (default 0 if no universe stashed → the checks fail loudly, which is
  correct: no universe at 09:16 = broken). Add both counts to the existing `info!` PROOF line.

## Edge Cases

- No universe stashed (boot failed) → counts 0 → both checks fail → Degraded page (correct: universe
  missing at market open is a real problem the operator must see).
- NTM source degraded that day (`NTM-CONSTITUENCY-01` already paged at boot) → ntm count ~0 < floor →
  ntm_universe_complete fails → the 09:16 health snapshot accurately reflects the degraded universe.
  This is once-per-day (not Rule-4 polling spam) and is the authoritative current-state check.
- Legit single-index delisting → index count 32 ≥ 30 floor → no false page.
- All green → SELFTEST-01 Passed with checks_passed = 8 (positive daily proof the universe is whole).

## Failure Modes

- Pure evaluator — no I/O, no panic paths, zero alloc on the all-green path (existing contract kept).
- Self-test is observe-only: it ALERTS, never halts the feed (existing design).
- main.rs read of `stashed_universe()` is `Option` — `None` maps to 0 counts, never panics.

## Test Plan

- `cargo test -p tickvault-core --lib instrument::market_open_self_test` — new + existing unit tests:
  pass-when-complete (checks_passed=8), degraded-when-index-below-floor,
  degraded-when-ntm-below-floor, both-below-floor lists both names + stays Degraded (not Critical),
  updated constants-pinned (TOTAL_SUB_CHECKS=8, 2 floors), green baseline updated.
- `cargo test -p tickvault-app` — main.rs compiles; feature_flag_rollback_guard unaffected.
- banned-pattern + pub-fn-test + plan-verify + plan-gate green.

## Rollback

- `git revert` removes the 2 fields + 2 checks + wiring; the self-test reverts to 6 checks; the feed
  and universe build are untouched (self-test never gated them). Clean, single-PR revert.

## Observability

- Reuses SELFTEST-01 (Info/pass) + SELFTEST-02 (High/Degraded) — existing Telegram + `tv_self_test_total`
  counter + the runbook in `wave-3-c-error-codes.md` (already documents SELFTEST-01/02). The two new
  sub-check names appear in the SELFTEST-02 `failed` list. main.rs `info!` PROOF line adds
  `index_values` + `ntm_constituents` counts.

## Plan Items

- [x] Item 1 — `market_open_self_test.rs`: 2 fields + 2 floor consts + 2 sub-checks + TOTAL_SUB_CHECKS 6→8 + tests
  - Files: `crates/core/src/instrument/market_open_self_test.rs`
  - Tests: test_self_test_passes_when_universe_complete, test_self_test_degraded_when_index_universe_below_floor, test_self_test_degraded_when_ntm_universe_below_floor, test_self_test_universe_checks_are_not_critical
- [x] Item 2 — wire the 2 inputs from the stashed universe in the 09:16 self-test closure
  - Files: `crates/app/src/main.rs`
  - Tests: test_self_test_constants_pinned (updated floors/count assertions)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy day: 33 indices + 748 NTM live | SELFTEST-01 Passed, checks_passed=8 (positive proof) |
| 2 | NTM constituents missing (niftyindices broke) | SELFTEST-02 Degraded names `ntm_universe_complete`; feed runs |
| 3 | Index universe collapsed (< 30) | SELFTEST-02 Degraded names `index_universe_complete` |
| 4 | One index legitimately delisted (32 left) | no false page (≥ 30 floor) |
