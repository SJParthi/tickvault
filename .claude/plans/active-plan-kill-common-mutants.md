# Implementation Plan: B6 — kill the 47 surviving tickvault-common mutants (run 28655302316)

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban — standing autonomy directive 2026-07-03

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (see the Per-Item Guarantee Matrix table below).

## Design

The newly-unmasked mutation gate (workflow run 28655302316 on main @
`c43c7568`, cancelled at ~195/5059 mutants) reported 47 MISSED mutants in
`tickvault-common`, spread across 4 files: `crates/common/src/candle_fold.rs`,
`config.rs`, `constants.rs`, `disconnect_cause.rs`. Each survivor is
classified KILLABLE (a test can assert the exact behavior the mutant changes)
or EQUIVALENT (the mutated program is runtime-identical, so no test can ever
kill it), then:

- **KILLABLE** → add boundary / exact-value / exact-string tests in
  `tickvault-common` (in-module `#[test]`, matching existing conventions).
- **EQUIVALENT** → document + exclude via a new `.cargo/mutants.toml`
  (`exclude_re`), each entry carrying its equivalence proof. NO production
  code is weakened; NO new crate dependency is added.
- **Wall-clock-dependent (config.rs:1369 `+`→`-`, :1379 `<` swaps)** →
  neither equivalent nor deterministically testable inline (the mutant is
  only observable within 5h30m of an IST midnight, or on the exact earliest
  date). Fix: extract two PURE private helpers in `config.rs` —
  `ist_date_from_utc(DateTime<Utc>) -> NaiveDate` and
  `is_before_live_trading_earliest(NaiveDate, NaiveDate) -> bool` — used by
  `ApplicationConfig::validate`, behavior-preserving, and test them
  deterministically at boundaries.

Classification result:

| Survivor group | Count | Verdict |
|---|---|---|
| `candle_fold.rs:162` `>`→`>=` in `OneMinFoldCell::consume` | 1 | EQUIVALENT — the `else if minute_start > self.minute_start_nanos` arm is only reachable when the preceding `==` arm did not match; on the reachable domain (`!=`) `>` ≡ `>=`. Excluded. |
| `config.rs:435` `&&`→`\|\|` + delete `!` in `check_sandbox_window` | 2 | KILLABLE — Live + dry_run before cutoff must STILL be blocked. |
| `config.rs:577` `default_activity_watchdog_threshold_secs` → 0/1 | 2 | KILLABLE — exact pinned value 50. |
| `config.rs:631` `default_retention_days` → 0/1 | 2 | KILLABLE — exact pinned value 90. |
| `config.rs:901` `effective_main_feed_pool_size` → 1 | 1 | EQUIVALENT — the fn body IS `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1`, so "replace with 1" is a no-op. Excluded (caveat documented: delete the entry if the constant ever changes). |
| `config.rs:1369` `+`→`-` (IST offset) | 1 | KILLABLE after helper extraction — `ist_date_from_utc` boundary tests. |
| `config.rs:1379` `<`→`==`/`<=` (earliest-date gate) | 2 | KILLABLE after helper extraction — `is_before_live_trading_earliest` three-point boundary test. |
| `constants.rs:1001-1006` `&&`→`\|\|` inside the compile-time-only `const _: () = {...}` INDIA-VIX assertion block | 6 | EQUIVALENT — the block evaluates to `()`; any mutation either fails the build (unviable = caught) or is a runtime no-op. Nothing observable exists to test. Excluded with `$`-anchored regex that cannot touch function-scoped `&&` mutants. |
| `constants.rs:1094/1746/1751/1907/1922/1969/1978` arithmetic swaps in const initializers | 24 | KILLABLE — exact computed-value asserts (52_428_800 / 80_000 / 104_857_600 / 604_800 / 43 / 14_400 / 14_400); every operator swap yields a different value. |
| `disconnect_cause.rs:53/66/90` `label`/`explanation`/`confirm_hint` → "xyzzy" | 6 (3 fns × ""/"xyzzy"; the "" ones were already killed by the non-empty check) | KILLABLE — exact-string asserts for all 6 variants × 3 fns. |

Proof protocol: cargo-mutants run locally, scoped with `-F` regexes to exactly
the affected functions/lines (81 mutants in scope), BEFORE (on origin/main
state — survivors present) and AFTER (0 MISSED, equivalents excluded).

## Edge Cases

- `check_sandbox_window`: Live + dry_run=true (the mutant-escape combination)
  must Err before the cutoff; non-live + dry_run stays Ok (first early-exit).
- `OneMinFoldCell::consume`: last nanosecond of the open minute folds in
  place; first nanosecond of the next minute seals; strictly earlier minute
  discards — pins the three-way comparison routing at the exact boundary.
- `ist_date_from_utc`: 20:00 UTC crosses IST midnight forward (next date);
  05:00 UTC stays same date (would regress to previous date under `-`).
- `is_before_live_trading_earliest`: day-before (blocks), the earliest date
  itself (ALLOWED — strict `<`, kills `<=`), day-after (allowed — kills `==`
  and `>`).
- Const-initializer arithmetic: every operator swap was hand-verified to
  produce a value different from the pinned assert (e.g. all 8 swaps in
  `STOCK_CONTRACTS_PER_EXPIRY` yield ≠ 43).
- Exclusion regex safety: `$` anchor means function-scoped mutants (which end
  with ` in <fn>`) can NEVER be silently excluded by the constants.rs
  const-block entry.

## Failure Modes

- A future `PHASE_0_MAIN_FEED_CONNECTION_COUNT != 1` change would make the
  `effective_main_feed_pool_size -> 1` exclusion mask a real mutant → the
  exclusion entry carries an explicit CAVEAT comment instructing deletion.
- A future value-producing `const X: bool = a && b;` in constants.rs would be
  excluded by the const-block regex → documented in the mutants.toml comment
  with the mitigation (use named `const fn`s for such logic).
- Helper extraction in `validate()` is behavior-preserving by construction
  (same expression, same operands); the full workspace test battery + the
  existing sandbox-guard tests gate any regression.
- Test-count guard ratchet: count INCREASES (allowed direction).

## Test Plan

- New killing tests (14 total): `candle_fold.rs` (1), `config.rs` (7),
  `constants.rs` (7), `disconnect_cause.rs` (3) — see Plan Items for names.
- `cargo mutants -p tickvault-common -F <scoped>` BEFORE and AFTER; AFTER
  must show 0 MISSED (equivalents excluded via `.cargo/mutants.toml`).
- `cargo test --workspace` (crates/common change → workspace escalation per
  `testing-scope.md`), `cargo fmt --check`, CI-exact clippy,
  banned-pattern-scanner, plan-verify, per-item-guarantee-check, plan-gate.

### PROOF — cargo-mutants scoped summaries (verbatim)

BEFORE (origin/main @ 8ec44176, 81 mutants in scope):

```
Found 81 mutants to test
outcomes.json: {'Success': 1, 'CaughtMutant': 12, 'MissedMutant': 42, 'Unviable': 26}
missed.txt: 42 MISSED — candle_fold.rs:162 (> with >=), config.rs:435 (&& / delete !),
:577 (->0/->1), :631 (->0/->1), :901 (->1), :1369 (+ with -), :1379 (< with ==/<=),
constants.rs:1001-1006 (&& with ||, 6), :1094/:1746/:1751/:1907/:1922 (arithmetic, 22),
disconnect_cause.rs:53/66/90 (-> "xyzzy", 3)
```

(Note: the BEFORE process hit "No space left on device" while writing diff
artifacts AFTER all 81 outcomes were recorded — outcomes.json is complete;
the one-line console summary was therefore never printed.)

AFTER (this branch, same scope with the retired validate() line filters
replaced by the extracted helper names):

```
Found 73 mutants to test
73 mutants tested in 6m: 56 caught, 17 unviable
outcomes.json: {'Success': 1, 'CaughtMutant': 56, 'Unviable': 17}   # 0 MISSED
```

(81 → 73 mutants: 8 documented-equivalent mutants excluded via
`.cargo/mutants.toml`; the 5 old line-scoped `validate()` mutants were
replaced by the new pure helpers' mutants — all caught.)

## Rollback

Single revert of the one commit (tests + two pure private helpers +
`.cargo/mutants.toml`) restores the exact prior state. No schema, no config,
no runtime-behavior change ships in this PR; deleting `.cargo/mutants.toml`
alone restores the pre-exclusion mutation surface.

## Observability

No runtime behavior change — observability surface is the CI mutation gate
itself: the weekly/PR `mutation.yml` workflow now reports these mutants as
CAUGHT (or excluded-with-proof); cargo-mutants' non-zero exit on any MISSED
mutant (with pipefail) is the fail-loud signal for regression. (Follow-up
noted for the coordinator: the workflow's `grep -q "SURVIVED"` check step can
never match cargo-mutants' "MISSED" wording — the gate works only via the run
step's exit code.) The extracted helpers keep the existing
`bail!` → ERROR-log → Telegram path in `validate()` untouched.

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row
+ 7-row canonical matrices apply). Per-item specifics:

| Demand | This PR's proof |
|---|---|
| 100% testing coverage | 14 new killing tests; scoped mutation score for the target set = 100% (caught + excluded-equivalent) |
| 100% code checks | fmt + clippy + banned-pattern + plan-verify + plan-gate + guarantee-check all green |
| 100% functionalities covering | the two new private helpers are called by `validate()` and directly tested |
| 100% extreme check | the mutation gate itself is the ratchet — these mutants fail the build if the tests regress |
| Zero ticks lost / hot path | untouched — tests + two pure cold-path helpers only; no hot-path allocation added |
| Uniqueness + dedup | untouched |
| Honest envelope | "100% inside the tested envelope": scoped to the 47 surviving mutants of run 28655302316; equivalents are excluded with written proofs, not silenced |

## Plan Items

- [x] Item 1 — Classify all 47 survivors (KILLABLE vs EQUIVALENT)
  - Files: (analysis; recorded in this plan's Design table)
  - Tests: n/a

- [x] Item 2 — `.cargo/mutants.toml` with 3 documented equivalent-mutant exclusions
  - Files: .cargo/mutants.toml
  - Tests: n/a (config; proven by the AFTER cargo-mutants run)

- [x] Item 3 — candle_fold boundary test
  - Files: crates/common/src/candle_fold.rs
  - Tests: test_consume_minute_comparison_boundary

- [x] Item 4 — config.rs sandbox-window mutant kills + default-fn exact values
  - Files: crates/common/src/config.rs
  - Tests: test_sandbox_live_with_dry_run_still_blocks, test_sandbox_non_live_with_dry_run_ok_before_cutoff, test_default_activity_watchdog_threshold_secs_is_exactly_50, test_default_retention_days_is_exactly_90

- [x] Item 5 — extract pure sandbox-gate helpers + boundary tests
  - Files: crates/common/src/config.rs
  - Tests: test_ist_date_from_utc_crosses_midnight_forward, test_ist_date_from_utc_early_utc_hours_stay_same_day, test_is_before_live_trading_earliest_boundary

- [x] Item 6 — constants.rs exact-computed-value tests for const-initializer arithmetic
  - Files: crates/common/src/constants.rs
  - Tests: test_max_csv_body_bytes_is_exactly_50_mib, test_tick_buffer_high_watermark_is_exactly_80_percent, test_tick_spill_min_disk_space_is_exactly_100_mib, test_spill_file_max_age_is_exactly_7_days, test_stock_contracts_per_expiry_is_exactly_43, test_token_sweep_interval_is_exactly_4_hours, test_token_sweep_staleness_threshold_is_exactly_4_hours

- [x] Item 7 — disconnect_cause exact-string tests
  - Files: crates/common/src/disconnect_cause.rs
  - Tests: test_label_exact_strings_kill_mutants, test_explanation_exact_strings_kill_mutants, test_confirm_hint_exact_strings_kill_mutants

- [x] Item 8 — BEFORE/AFTER scoped cargo-mutants proof runs + paste summaries above
  - Files: (this plan file)
  - Tests: n/a

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Any of the 38 killable mutants re-applied | at least one new test fails (mutant dies) |
| 2 | The 9 excluded-equivalent mutants | skipped by cargo-mutants with written proof in mutants.toml |
| 3 | `PHASE_0_MAIN_FEED_CONNECTION_COUNT` changes from 1 | caveat comment instructs deleting the exclusion |
| 4 | Workspace battery | all green; test count increases (ratchet-allowed direction) |
