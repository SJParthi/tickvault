# Implementation Plan: fix the 2026-08-07 audit findings (false-OK / false-RED sweep)

**Status:** VERIFIED
**Date:** 2026-08-07
**Approved by:** Parthiban (operator) — "fix evreryhtign rbo", in direct
response to the 8-agent audit summary listing these findings and to the
bench-gate option table where option A was recommended. Earlier same-session
directive: *"Ensure to cover all kinds of 100 percentage stricter hardening
test cases ... with real time guarantee and assurnace"* and *"do you understand
what in askign dude?"*.

## Scope

Four verified defects, all of the same family the operator has been burned by
all session: **a signal that does not mean what it says.** Two are false-OK
(a check that always passes), one is a false-RED (a check that always fails),
one is documentation asserting things the code does not do.

| # | Defect | Class | Evidence (verified this session) |
|---|---|---|---|
| 1 | `bench-gate.sh` regression arm fails on shared-runner hardware drift | **false-RED** | 31/31 benches "regressed" +24–30% uniformly on `993a144c`; every absolute budget PASSED; same SHA `da796888` passed Aug 5, failed Aug 7 |
| 2 | `test-count`, `pub-fn-test`, `financial` guards pass vacuously | **false-OK** | baselines gitignored (`.gitignore:70-73`); all three `exit 0` after writing a fresh baseline when the file is missing |
| 3 | 6 core tests gate on credential PRESENCE, not SSM reachability | **fragile test** | `test_support.rs:14-29` returns true on a bare `AWS_ACCESS_KEY_ID`; the 6 tests then assert "fetch should succeed" and panic wherever SSM is unroutable |
| 4 | CLAUDE.md asserts facts the code contradicts | **doc lie** | `InstrumentRegistry` is `HashMap` (`instrument_registry.rs:159`) not papaya; papaya pinned 0.2.4 not 0.2.3; "100% code coverage" vs enforced floors 63–99.5 |

**Explicitly NOT in scope** (each needs an operator decision, not code):
`All Green` branch-protection click; the `main.tf:77` single-AZ pin.

## Design

**Fix 1 — hardware-drift detection in the regression arm.**
A genuine code regression hits a *few related* benchmarks. A hardware/toolchain
change hits *all* of them by *the same* amount. So: collect every confident
regression, and if the regressing share is at or above
`HARDWARE_DRIFT_MIN_SHARE` (0.70) of all compared benchmarks **and** their
spread is tight (max−min ≤ `HARDWARE_DRIFT_SPREAD_PCT`, 15 points), classify
as DRIFT — print every line loudly as `DRIFT:` plus a one-line verdict, and do
not fail the regression arm. Anything else keeps failing exactly as today.
Absolute budgets are untouched and still hard-fail (exit 1) in all cases.
This is strictly *more* discriminating than the status quo: it does not widen
the 5% threshold and does not delete the check, so a real 30% regression in
1–5 benches still fails. Chosen over (B) reset baseline — breaks again on the
next runner rotation — and (C) widen to 35% — blinds the gate to real
regressions.

**Fix 2 — make the ratchets real.** Un-gitignore the three baseline files and
commit them at their current measured values, so CI has something to compare
against. Belt-and-braces: when `CI` is set and the baseline is missing, the
guards now **fail closed** (exit 1) instead of silently establishing one.
Local first-run behaviour is unchanged.

**Fix 3 — opt-in real-SSM tests.** New `real_ssm_tests_enabled()` requires an
explicit `TICKVAULT_TEST_REAL_SSM=1` **and** credentials. The 6 call sites move
to it. Without the opt-in they assert the true unit contract (returns `Err`,
never panics), which holds in every environment. `has_aws_credentials()` stays
(its own tests are unchanged).

**Fix 4 — correct the docs.** Fix the four false claims in CLAUDE.md; state
enforced coverage floors with 100% as the stated target; mark mutation/fuzz/
sanitizers scheduled-only; qualify the blanket O(1) claim to name the
`SpotBarStore` O(log n)/O(n) reality the code itself already documents.

## Edge Cases

- Drift detector with 0 or 1 compared benchmarks → share arithmetic must not
  divide by zero; treat as NOT drift (fail-safe toward the old behaviour).
- Exactly one benchmark regressing in a 1-benchmark run → 100% share but a real
  regression; the tight-spread test alone would pass it, so require a minimum
  absolute count (`HARDWARE_DRIFT_MIN_COUNT` = 5) before drift may be declared.
- A real regression *coinciding* with hardware drift → honestly unresolvable
  from relative data alone; absolute budgets remain the backstop and the DRIFT
  verdict line says so explicitly.
- Guard baselines: a developer with no baseline and `CI` unset keeps today's
  auto-establish; with `CI=1` set locally they get the fail-closed path.
- `TICKVAULT_TEST_REAL_SSM=1` set but credentials absent → do not run the
  real-SSM branch (both conditions required).

## Failure Modes

- Drift detector too permissive → a genuine broad regression is dismissed.
  Mitigated by the absolute budgets (unchanged, hard-fail) and by requiring
  BOTH high share AND tight spread AND ≥5 benches.
- Drift detector too strict → false RED returns. Mitigated by the ratchet test
  replaying the real 2026-08-07 numbers.
- Committed baselines drift from reality → guards nag. Accepted: that is the
  ratchet working; the fix is to update the committed number in the PR.
- Opt-in gating hides a real SSM regression → accepted; those assertions were
  never a unit-test contract, and CI has no AWS account either way.

## Test Plan

- `scripts/bench-gate.selftest.sh` — new: (a) the real 31-bench 2026-08-07
  drift fixture → exit 0 with a DRIFT verdict; (b) 3-of-31 regressing → exit 2
  (still fails); (c) drift-shaped share but wide spread → exit 2; (d) fewer
  than 5 benches → exit 2; (e) absolute breach during drift → exit 1.
- `crates/common/tests/audit_fix_guard.rs` — ratchets: the three baselines are
  tracked in git and absent from `.gitignore`; each guard contains the
  CI-fail-closed branch; `bench-gate.sh` carries the drift constants.
- `crates/core/src/test_support.rs` unit tests for `real_ssm_tests_enabled()`
  (both-set / opt-in-only / creds-only / neither).
- Re-run the 6 previously-failing core tests → expect pass with no opt-in.
- Per-crate suites for every crate touched; `cargo fmt`; `cargo clippy`.

## Rollback

Every change is additive or a literal revert:
- Fix 1: revert `bench-gate.sh`; the regression arm returns to today's
  behaviour. No data migration.
- Fix 2: re-add the four `.gitignore` lines and `git rm --cached` the
  baselines. Guards return to auto-establish.
- Fix 3: revert the 6 call sites to `has_aws_credentials()`.
- Fix 4: docs only, revert freely.
No config flag, no schema change, no runtime behaviour change in the trading
path — nothing here ships to the prod box.

## Observability

- Bench gate prints `DRIFT:` per line plus an explicit verdict naming the share,
  the spread, and that absolute budgets still gate — so a drift-classified run
  is never silently green; the reader is told exactly what was suppressed.
- Guards print which baseline they compared against and its value.
- The CI-missing-baseline path prints a named error telling the operator to
  commit the baseline, rather than passing.

## Plan Items

- [x] Item 1 — bench-gate hardware-drift detection
      - Files: `scripts/bench-gate.sh`, `scripts/bench-gate.selftest.sh`
      - Tests: the 5 selftest cases above
- [x] Item 2 — commit the three ratchet baselines + CI fail-closed
      - Files: `.gitignore`, `.claude/hooks/.test-count-baseline`,
        `.claude/hooks/.untested-pubfn-baseline`,
        `.claude/hooks/.financial-test-baseline`,
        `.claude/hooks/{test-count,pub-fn-test,financial-test}-guard.sh`
      - Tests: `crates/common/tests/audit_fix_guard.rs`
- [x] Item 3 — opt-in real-SSM test gating
      - Files: `crates/core/src/test_support.rs`,
        `crates/core/src/auth/secret_manager.rs`,
        `crates/core/src/network/ip_verifier.rs`,
        `crates/core/src/notification/service.rs`
      - Tests: `real_ssm_tests_enabled` unit tests + the 6 repaired tests
- [x] Item 4 — correct the four false claims in CLAUDE.md
      - Files: `CLAUDE.md`
      - Tests: `crates/common/tests/audit_fix_guard.rs` (papaya claim absent)

## Per-Item Guarantee Matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md`, cross-referenced in
full. Item-specific notes:

| Demand | This plan |
|---|---|
| 100% code coverage | No new production code paths; the new test-support fn is unit-tested; floors unchanged |
| 100% audit coverage | N/A — no new typed event, no table |
| 100% testing coverage | unit + selftest (differential fixtures) + source-scan ratchets |
| 100% code checks | fmt + clippy + banned-pattern + the repaired guards themselves |
| 100% code performance | N/A — no hot-path code touched; DHAT unchanged |
| 100% monitoring | N/A — no runtime component |
| 100% logging | gate/guard stdout only, no `error!` sites added |
| 100% alerting | N/A — no new failure mode reaches prod |
| 100% security | no secret handling changed; opt-in env var carries no credential |
| 100% security hardening | attack surface delta: zero (no runtime code) |
| 100% bugs fixing | this plan IS the bug fix; 4 defects with evidence |
| 100% scenarios covering | 5 drift fixtures + 4 opt-in permutations + CI/local baseline paths |
| 100% functionalities covering | every new fn has a test AND a call site |
| 100% code review | adversarial pass on the diff before PR |
| 100% extreme check | ratchets fail the build on regression of all four fixes |

## Resilience Matrix (7-row)

| Demand | This plan |
|---|---|
| Zero ticks lost | No tick path touched — zero delta |
| WS never disconnects | No WS code touched |
| Never slow/locked/hanged | No hot-path code; no new allocation |
| QuestDB never fails | No storage code touched |
| O(1) latency | No runtime complexity change; Fix 4 *corrects* the O(1) claim |
| Uniqueness + dedup | No DEDUP key touched |
| Real-time proof | Gate/guard output is the proof surface; drift verdict is explicit |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the drift
detector is proven against the real 2026-08-07 31-benchmark fixture and against
four negative fixtures that must still fail; the three baselines are proven
tracked-in-git by a source-scan ratchet; the opt-in gating is proven by unit
tests over all four env permutations. **NOT claimed:** that the drift detector
can separate a genuine broad regression that happens to coincide with a
hardware rotation — relative data alone cannot, and the absolute budgets are
the stated backstop; that committing the baselines makes the three guards run
in CI (they are wired local-only today — wiring them into Repo Guards is a
deliberate follow-up, called out so nobody reads this PR as delivering it);
that CLAUDE.md is now free of every inaccuracy — only the four verified claims
are corrected.

## Auto-driver explanation

> Sir, three problems, all the same kind — a signal that lies. First: the shop's
> stopwatch was swapped for a slower one, so every worker looked 29% slower and
> the alarm screamed. We taught the alarm one rule: if EVERYONE slowed by the
> SAME amount, it is the clock, not the workers — say so out loud and check the
> real deadline instead, which everyone still beats. Second: three inspectors
> whose notebooks got thrown away nightly, so every morning they started blank
> and declared "no change." We stopped throwing the notebooks away. Third: a
> test that asked "do you have a shop key?" when it meant "is the shop
> reachable?" — now it asks the right question. And fourth, we corrected the
> signboard that claimed things the shop does not actually do.
