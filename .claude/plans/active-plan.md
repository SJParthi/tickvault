# Implementation Plan: NTM subscription-contract ratchet (build-failing guard for the 33/748/NTM-only invariants)

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — "go ahead" → AskUserQuestion answer "Harden tickvault further".
**Crate(s) touched:** `core` (new test-only file `crates/core/tests/ntm_subscription_contract_guard.rs`).
No production code changes. No new ErrorCode.

## Context

The runtime F2 self-test (#1047) catches universe drift at 09:16 IST. This adds the COMPILE/CI-time
counterpart: a ratchet that fails the build the instant anyone silently changes the NTM subscription
contract — the numbers the operator cares about (33 index values, ~748 NTM stocks, NTM-UNION-ONLY
subscription, cap headroom). The biggest unpinned risk: `NTM_CONSTITUENCY_SLUGS` contents are
asserted NOWHERE today, so a future edit could expand the subscribed constituent set from "Nifty
Total Market only" to all ~46 index lists (~the entire NSE), breaching `MAX_DAILY_UNIVERSE_SIZE` and
the 2-WS lock. This guard pins that decision + the cross-constant relationships that no single
existing test covers.

## Design

New `crates/core/tests/ntm_subscription_contract_guard.rs` with pure const-relationship assertions:

1. `subscription_slug_is_ntm_only` — `NTM_CONSTITUENCY_SLUGS == [("Nifty Total Market",
   "ind_niftytotalmarket_list")]` (exactly one; the subscribed constituent source is NTM only).
2. `ntm_subscription_slug_is_subset_of_full_map` — every `NTM_CONSTITUENCY_SLUGS` entry also exists
   in `INDEX_CONSTITUENCY_SLUGS` (the map-only superset).
3. `full_constituency_map_includes_ntm` — `INDEX_CONSTITUENCY_SLUGS` contains the NTM entry.
4. `index_value_allowlist_includes_ntm` — `NSE_INDEX_ALLOWLIST` contains "NIFTY TOTAL MKT".
5. `f2_index_floor_within_achievable_bounds` — `1 <= INDEX_VALUES_SUBSCRIBED_FLOOR <=
   NSE_INDEX_ALLOWLIST.len() + 1` (the +1 = BSE SENSEX = 33 max). Catches a floor set above what's
   achievable (would page every day).
6. `f2_ntm_floor_fits_under_cap` — `1 <= NTM_CONSTITUENTS_SUBSCRIBED_FLOOR` AND
   `(NSE_INDEX_ALLOWLIST.len()+1) + NTM_CONSTITUENTS_SUBSCRIBED_FLOOR <= MAX_DAILY_UNIVERSE_SIZE`.
7. `universe_cap_accommodates_expected_ntm_union` — documented expected counts
   (EXPECTED_INDEX_VALUES = 33, EXPECTED_NTM_CONSTITUENTS = 748) satisfy
   `EXPECTED_INDEX_VALUES + EXPECTED_NTM_CONSTITUENTS <= MAX_DAILY_UNIVERSE_SIZE` AND
   `MIN_DAILY_UNIVERSE_SIZE <= EXPECTED_INDEX_VALUES` (index-only fallback still clears the min gate).

All assertions are deterministic const reads — no I/O, no feature flag, no network. Not duplicating
existing single-value pins (`MAX_DAILY_UNIVERSE_SIZE==1200`, allowlist len==32, F2 floors) — this
guard pins the RELATIONSHIPS between them + the previously-unpinned NTM-only slug contract.

## Edge Cases

- Someone adds a 2nd slug to `NTM_CONSTITUENCY_SLUGS` → test 1 fails (the exact regression that would
  blow the cap).
- Someone lowers `MAX_DAILY_UNIVERSE_SIZE` below the live union (781) → test 6/7 fail.
- Someone raises an F2 floor above achievable → test 5/6 fail.
- Someone removes NTM from the allowlist or the map → test 3/4 fail.

## Failure Modes

- Test-only file — cannot affect production behavior. Worst case is a false-positive build break,
  resolved by reconciling the guard with an intentional contract change (which SHOULD be a conscious,
  reviewed edit — that's the point).

## Test Plan

- `cargo test -p tickvault-core --test ntm_subscription_contract_guard` — all 7 assertions green now.
- banned-pattern + plan-verify + plan-gate green.

## Rollback

- `git revert` deletes the test file; zero production impact.

## Observability

- N/A — compile/CI-time guard. Its "signal" is a red build on contract drift (the strongest signal
  per the charter's "ratchet test fails the build on regression" demand).

## Plan Items

- [x] Item 1 — add `ntm_subscription_contract_guard.rs` with the 7 cross-constant ratchet tests
  - Files: `crates/core/tests/ntm_subscription_contract_guard.rs`
  - Tests: subscription_slug_is_ntm_only, ntm_subscription_slug_is_subset_of_full_map, index_value_allowlist_includes_ntm, universe_cap_accommodates_expected_ntm_union

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Someone expands NTM subscription to all 46 lists | build FAILS (subscription_slug_is_ntm_only) |
| 2 | Someone lowers the universe cap below 781 | build FAILS (universe_cap_accommodates_expected_ntm_union) |
| 3 | Someone drops NTM from the allowlist/map | build FAILS (index_value_allowlist_includes_ntm / full_constituency_map_includes_ntm) |
| 4 | Healthy contract | all 7 green |
