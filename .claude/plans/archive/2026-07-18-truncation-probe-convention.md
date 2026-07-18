# Implementation Plan: Unify /exec truncation guards to the LIMIT+1 probe convention

**Status:** VERIFIED
**Date:** 2026-07-18
**Approved by:** Parthiban (operator, via coordinator register item 2026-07-18)

## Plan Items

- [x] Convert the `crates/app` (tickvault-app) rest_candle_fold `/exec` probes
  (`spot_bars_sql` + `parse_spot_bars`, `spot_discovery_sql` +
  `parse_spot_discovery`) from the refuse-at-`>= limit` shape to the house
  LIMIT+1 probe (#1630 precedent): SQL fetches `limit + 1`, `truncated` only
  when `len > limit`, exact-boundary datasets trusted as complete.
  - Files: crates/app/src/rest_candle_fold.rs
  - Tests: test_spot_bars_sql_shape, test_spot_discovery_sql_shape,
    test_parse_spot_bars_and_truncation_tripwire,
    test_parse_spot_discovery_segment_allowlist_and_truncation
- [x] Convert the tf_consistency `/exec` probes (`select_1m_sql`,
  `select_tf_union_sql`, `select_instruments_sql` + `parse_1m_dataset`,
  `parse_tf_union_dataset`, `parse_instruments_dataset`) to the same LIMIT+1
  probe, preserving the GIFT-Nifty always-on prefix argument (in-session
  rows ≤ 375 < the 500 cap, so a `> cap` response still carries
  out-of-session rows in the ORDER BY ts ASC prefix).
  - Files: crates/app/src/tf_consistency_boot.rs
  - Tests: test_select_1m_sql_shape (LIMIT 501),
    test_select_tf_union_sql_wraps_limit (LIMIT 2001),
    test_select_instruments_sql_shape (LIMIT 3001),
    test_parse_1m_dataset_happy_path_skips_malformed_flags_cap,
    test_parse_tf_union_dataset_groups_by_label_and_flags_cap,
    test_parse_instruments_dataset_rows_and_cap,
    test_truncated_always_on_prefix_still_excluded_not_degraded
- [x] Add the shared-convention ratchet: a source-scan guard pinning the
  LIMIT+1-probe needles at ALL five `/exec` completeness-probe sites
  (rest_candle_fold, tf_consistency_boot, market_ram_store_boot,
  spot_crossverify_boot, groww_spot_1m_boot) and banning the `>= limit`
  refusal shape from their production regions.
  - Files: crates/app/tests/truncation_probe_convention_guard.rs
  - Tests: probe_sites_fetch_limit_plus_one, probe_sites_flag_strictly_over_limit,
    no_refuse_at_limit_comparison_remains, scanner_self_test_detects_violation
- [x] Rule-file truth-sync (same PR, dated 2026-07-18): update the documented
  `>= limit` semantics in rest-candle-fold-error-codes.md (`catchup_parse` /
  `discovery_truncated` LIMIT wording) and tf-consistency-error-codes.md
  (`truncated` stage wording). ram-store-error-codes.md already documents the
  target convention — unchanged.
  - Files: .claude/rules/project/rest-candle-fold-error-codes.md,
    .claude/rules/project/tf-consistency-error-codes.md
  - Tests: n/a (docs)

## Design

One convention for every QuestDB `/exec` completeness probe in the app crate:
the query asks for `cap + 1` rows (`saturating_add(1)` in the SQL builder),
the parser flags `truncated` ONLY when the dataset carries MORE than `cap`
rows (`len > cap`), and an exact-boundary `len == cap` dataset is trusted as
legitimately complete. This is the #1630 (spot_crossverify) / PR-2 round-1
(market_ram_store) shape; the drift sites are rest_candle_fold (both probes)
and tf_consistency_boot (all three probes), which query exactly `LIMIT cap`
and refuse at `len >= cap` — a zero-headroom false-PARTIAL at any legitimate
exactly-cap day. Row handling stays site-specific and documented: the fold
and tf-union callers DISCARD rows on truncation; tf_consistency Query A
keeps the truncated rows for the always-on exclusion (unchanged, refuter
round 3); spot_crossverify trims to `cap` (unchanged). A new source-scan
ratchet (house production-region-split style) pins the convention at all
five sites so a sixth site or a regression re-introducing `>= limit` fails
the build.

## Edge Cases

- Exactly-cap datasets (500 fold 1m rows / 64 discovery pairs / 500 tf 1m /
  2,000 tf-union / 3,000 tf discovery): now COMPLETE, not truncated — the
  zero-headroom false-flag class #1630 fixed for spot_crossverify.
- cap+1 datasets: truncated on every site (the probe row proves overflow).
- GIFT-Nifty ~21h always-on day (~1,260 1m rows > the 500 cap): still
  parses `truncated = true` (1,260 > 500) and the returned rows still carry
  out-of-session rows in the ORDER BY ts ASC prefix (in-session ≤ 375 <
  500), so the H2 exclusion keeps firing BEFORE the truncation degrade.
- `usize::MAX` cap: `saturating_add(1)` never overflows (degenerate — the
  probe can then never flag; no production cap approaches it).
- The brutex_crossverify CSV accept-cap (`out.rows.len() >= max_rows` →
  stop accepting) is a STREAMING row cap, not a query completeness probe —
  deliberately out of scope and excluded from the guard (documented there).
- The market_ram_store chain_row_cap / minute-cap drops are row caps, not
  probes — untouched.

## Failure Modes

- A future probe site written with `>= limit` refusal: the new guard's
  negative scan fails the build (production region only, comment-stripped).
- A builder losing its `+1` fetch: the guard's positive needle for that
  file fails, and the per-file SQL-shape unit tests (LIMIT 501/65/2001/…)
  fail first.
- Guard scanner going vacuous: the embedded self-test plants a violating
  fixture and asserts detection.
- Behavior risk: on truncation the fold/tf callers already degrade loudly
  and discard — returning cap+1 rows to a discarding caller changes
  nothing; tf Query A's exclusion argument is re-verified by the updated
  always-on prefix test.

## Test Plan

- `cargo test -p tickvault-app` scoped run (rest_candle_fold +
  tf_consistency + the new guard + market_ram_store/spot_crossverify/
  groww_spot_1m untouched suites stay green).
- Updated SQL-shape pins: LIMIT 501 (fold 1m), LIMIT 65 (fold discovery),
  LIMIT 501 (tf 1m), LIMIT 2001 (tf union), LIMIT 3001 (tf discovery).
- Updated parser pins: `len == cap` → complete; `len == cap+1` → truncated,
  at every converted site.
- New guard: 4 tests (positive needles per site, negative `>= limit` ban,
  scanner self-test).
- `bash .claude/hooks/banned-pattern-scanner.sh` clean.

## Rollback

Single-commit revert (`git revert`) restores the prior refuse-at-limit
shape at the two drift sites; the three already-LIMIT+1 sites are not
behaviorally touched (guard-pinned only), so a revert cannot regress them.
No schema, no config, no wire-format change — SQL LIMIT values and a pure
boolean comparison only; QuestDB reads are cold-path.

## Observability

No new metrics/codes: the existing loud degrade paths are unchanged —
`tv_rest_candle_fold_errors_total{stage="catchup_parse"|"discovery_truncated"}`
(FOLD-01) and `tv_tf_verify_query_failures_total{stage="truncated"}`
(TF-VERIFY-02) keep firing on genuine `> cap` truncation; what changes is
that an exactly-cap complete day no longer false-fires them. Rule files
rest-candle-fold-error-codes.md + tf-consistency-error-codes.md carry the
dated 2026-07-18 wording update in the same PR.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Fold catch-up day with exactly 500 1m rows | complete fold, no degrade |
| 2 | Fold catch-up day with 501+ rows | `catchup_parse` degrade, day skipped |
| 3 | tf_verify SID with exactly 500 in-window 1m rows | compared, no `truncated` |
| 4 | GIFT-Nifty ~1,260-row day, empty always-on set | excluded via out-of-session prefix, never degraded |
| 5 | Discovery result exactly at cap (64 / 3,000) | complete, catch-up/verify proceeds |
| 6 | New code re-introducing `.len() >= limit` in a probe file | guard fails the build |
