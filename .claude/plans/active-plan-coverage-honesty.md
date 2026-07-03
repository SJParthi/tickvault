# Implementation Plan: B7 — Coverage honesty + Groww QuestDB CI lane

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator B7 directive, verbatim: "B7 · Coverage honesty + Groww CI lane — Branch claude/coverage-honesty. Replace every '100% coverage' claim with real enforced floors OR raise the floors (app crate truly enforces 63.3%); make the QuestDB-backed groww e2e actually run in a CI lane (it self-skips everywhere today).")

> Guarantee matrices: this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` for the mandatory
> 15-row 100% guarantee matrix + 7-row resilience demand matrix, with this
> item's specifics: docs/CI-only change — no hot path touched, no tick-drop
> path added, no DEDUP key touched; new ratchet test
> `coverage_claim_honesty_guard.rs` fails the build on regression.
> Honest claim wording: any "100% guarantee" here means 100% inside the tested
> envelope, with ratcheted regression coverage — the coverage floors are the
> real enforced numbers (app 63.3 … common 99.5); 100% remains the target,
> not the current floor.

## Design

The executable truth is already honest: `quality/crate-coverage-thresholds.toml`
enforces ratcheted per-crate line-coverage floors (default 63.0, common 99.5,
core 90.2, trading 96.9, storage 91.2, api 98.6, app 63.3), gated fail-closed by
`scripts/coverage-gate.sh` in the post-merge `coverage-and-perf` job of
`.github/workflows/ci.yml`. The FALSE surface is documentation claiming
"100% per crate / blocks merge if <100%", plus a `Makefile coverage` target that
uses a contradictory flat `--fail-under-lines 99`.

Four workstreams, one branch:

1. **Docs honesty:** replace every load-bearing "100% coverage enforced" claim
   with the canonical honest phrasing: "ratcheted per-crate line-coverage floors
   (`quality/crate-coverage-thresholds.toml`: app 63.3 · core 90.2 · storage 91.2
   · trading 96.9 · api 98.6 · common 99.5 · default 63.0; floors only move up,
   100% remains the target), enforced post-merge by `scripts/coverage-gate.sh`".
   Historical archives and dated verbatim operator quotes are NOT touched.
2. **Makefile:** `make coverage` runs the SAME per-crate gate as CI
   (llvm-cov JSON → `scripts/coverage-gate.sh`), then still produces the HTML
   report.
3. **Groww e2e hard-fail switch:** `crates/app/tests/groww_live_pipeline_e2e.rs`
   gains a `TV_REQUIRE_QDB` env switch — when set, an unreachable QuestDB
   PANICS instead of skipping, so a CI lane can never vacuously pass.
4. **New CI lane** `.github/workflows/groww-e2e.yml` (dispatch + Sunday-night
   cron + path-filtered push on main): `docker run` of the SAME pinned
   `questdb/questdb:9.3.5` image the compose file uses (docker run, not
   compose, because the compose service pins `platform: linux/arm64` for prod
   Graviton parity and exec-format-fails on amd64 ubuntu-latest runners),
   readiness wait, run the e2e with `TV_REQUIRE_QDB=1`, then anti-false-OK
   asserts (no `SKIP ` line; ≥3 `PROVED:` lines), always-teardown
   (`docker rm -f`) + log artifact.

A new source-scan ratchet `crates/storage/tests/coverage_claim_honesty_guard.rs`
pins all of the above so the false claims cannot silently return
(audit-findings Rule 11: no false-OK signals).

## Edge Cases

- Other `active-plan*.md` files exist — this plan is a NEW file, none clobbered.
- `coverage_threshold_lockdown.rs` PINNED_CRATE_FLOORS must keep matching the
  TOML — the TOML is NOT changed by this plan.
- Operator-charter verbatim quote sections are NEVER edited; only the
  mechanical-matrix cell text in §C / enforcement-chain rows is corrected.
- The compose tv-questdb service pins `platform: linux/arm64` (prod Graviton
  parity) which cannot execute on amd64 CI runners — the lane therefore
  `docker run`s the same pinned image tag directly (native amd64 from the
  multi-arch manifest; the compose file carries no sha256 digest), so
  Loki/Alloy also don't waste CI minutes.
- The e2e without `TV_REQUIRE_QDB` keeps today's SKIP-and-return behaviour so
  local `cargo test` on a machine without Docker stays green.
- `set -o pipefail` in the workflow run step so a cargo failure is not masked
  by `tee`.

## Failure Modes

- QuestDB fails to come up in CI → readiness loop fails the job after ~180s
  with container logs printed (fail-closed, no vacuous green).
- QuestDB up but tests skip anyway (e.g. port mapping regression) →
  `TV_REQUIRE_QDB=1` panic + the `grep "^SKIP "` guard both fail the job.
- Tests pass but print fewer than 3 `PROVED:` lines (a test deleted/renamed) →
  the `PROVED:` count assertion fails the job.
- Someone re-adds `--fail-under-lines` or the "100% thresholds" wording →
  `coverage_claim_honesty_guard.rs` fails the build.
- `make coverage` gate failure → non-zero exit propagates (recipe lines fail
  the make target).

## Test Plan

- `cargo test -p tickvault-storage --test coverage_claim_honesty_guard` — new
  ratchet green.
- `cargo test -p tickvault-storage --test coverage_gate_guard` and
  `cargo test -p tickvault-common --test coverage_threshold_lockdown` —
  unchanged behaviour, still green.
- Groww e2e: with local QuestDB reachable run
  `TV_REQUIRE_QDB=1 cargo test -p tickvault-app --features daily_universe_fetcher --test groww_live_pipeline_e2e -- --nocapture`
  → 3 passed + 3 `PROVED:` lines. If QuestDB genuinely cannot start here,
  prove BOTH branches instead: SKIP path passes without the env var AND the
  `TV_REQUIRE_QDB=1` run FAILS with the panic message.
- Workflow YAML parses via `python3 -c "import yaml,...; yaml.safe_load(...)"`.
- `cargo fmt --check`, banned-pattern scanner, per-item-guarantee-check,
  plan-verify all green.

## Rollback

Every change is additive or doc-level: `git revert` of any individual commit
restores the prior state. Deleting `.github/workflows/groww-e2e.yml` +
reverting the e2e test edit restores the skip-only behaviour. The honesty
ratchet is a test file — reverting it removes the pin without touching prod
code. No schema, no config, no hot path involved.

## Observability

- CI lane uploads `groww-e2e.log` as an artifact on every run and prints
  compose logs on readiness failure.
- The lane's anti-false-OK asserts convert silent skips into red CI
  (`::error::` annotations).
- The honesty ratchet converts documentation drift into a build failure.
- No new runtime code paths → no new ErrorCode / Prometheus counter / audit
  table needed (N/A — docs, Makefile, test harness, CI only).

## Plan Items

- [x] Item 1 — Coverage-claim honesty edits (docs + comments)
  - Files: .github/workflows/ci.yml, CLAUDE.md, docs/architecture/guarantees.md, docs/architecture/100-percent-compliance-audit.md, .claude/plans/100pct-audit-tracker.md, .claude/rules/project/wave-4-shared-preamble.md, .claude/rules/project/per-wave-guarantee-matrix.md, .claude/rules/project/z-plus-defense-doctrine.md, .claude/rules/project/observability-architecture.md, .claude/rules/project/zero-loss-guarantee-charter.md, .claude/rules/project/testing-scope.md, .claude/rules/project/testing.md, .claude/rules/project/operator-charter-forever.md
  - Tests: coverage_claims_in_ci_yml_are_honest, claude_md_does_not_claim_100pct_minimum_for_all_crates

- [x] Item 2 — `make coverage` enforces the same per-crate TOML floors as CI
  - Files: Makefile
  - Tests: makefile_coverage_target_uses_the_real_per_crate_gate

- [x] Item 3 — Groww e2e hard-fails under TV_REQUIRE_QDB instead of skipping
  - Files: crates/app/tests/groww_live_pipeline_e2e.rs
  - Tests: groww_e2e_has_tv_require_qdb_hard_fail_branch

- [x] Item 4 — New CI lane for the QuestDB-backed Groww e2e
  - Files: .github/workflows/groww-e2e.yml
  - Tests: groww_e2e_workflow_exists_with_no_skip_guard_and_proved_assertion

- [x] Item 5 — Build-failing honesty ratchet
  - Files: crates/storage/tests/coverage_claim_honesty_guard.rs
  - Tests: coverage_claims_in_ci_yml_are_honest, claude_md_does_not_claim_100pct_minimum_for_all_crates, guarantees_doc_states_ratcheted_floors, makefile_coverage_target_uses_the_real_per_crate_gate, groww_e2e_workflow_exists_with_no_skip_guard_and_proved_assertion, groww_e2e_has_tv_require_qdb_hard_fail_branch

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Reader checks CLAUDE.md / guarantees.md coverage claim | Sees the real ratcheted floors + "100% is the target", not a false "100% enforced" |
| 2 | `make coverage` locally on a crate below its floor | Gate fails with the per-crate row named (same as CI) |
| 3 | CI lane runs with healthy QuestDB | 3 tests pass, 3 PROVED lines, artifact uploaded |
| 4 | CI lane runs but QuestDB dead | Job fails at readiness wait OR at the TV_REQUIRE_QDB panic — never vacuous green |
| 5 | Someone restores "100% thresholds" wording or `--fail-under-lines` | `coverage_claim_honesty_guard` fails the build |
