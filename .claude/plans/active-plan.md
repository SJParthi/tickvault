# Implementation Plan: P0 — Fail-closed coverage gate (fix vacuous pass)

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban (operator, 2026-06-10: "go ahead with the plan okay? … once it gets merged go ahead with the plan always" — P0 was presented as: fix the gate to match absolute paths, make it fail-closed, add a self-test so CI can never lie about coverage again; baseline thresholds per report P5 folded in to avoid re-creating the chronic-red Coverage & Perf incident fixed in #1072)
**Authority:** `.claude/plans/research/coverage-gaps.md` (merged via PR #1076) §2 GAP #0 + §6 P0/P5.

**Per-item guarantee matrix:** this plan cross-references the canonical 15-row +
7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` per its
"or cross-reference it" clause. Item-specific deltas: no hot-path code touched
(shell/python gate + thresholds TOML + one Rust integration test); no new pub
fn; no new dep; no schema change; no tick path.

## Plan Items

- [x] Fix `scripts/coverage-gate.sh` path matching + fail-closed
  - Files: scripts/coverage-gate.sh
  - Tests: coverage_gate_prints_rows_for_absolute_paths, coverage_gate_fails_closed_on_zero_matched_files (via guard test below)
  - Change 1: `re.match(r'crates/([^/]+)/', filename)` → `re.search(...)` so the
    absolute paths cargo-llvm-cov emits (`/home/runner/work/.../crates/...`) match.
  - Change 2: after aggregation, `if not crate_lines: print(error); sys.exit(1)`
    — the gate REFUSES a vacuous pass (audit-findings Rule 11: no false-OK).

- [x] Set per-crate thresholds to the REAL measured baseline (ratchet floors)
  - Files: quality/crate-coverage-thresholds.toml
  - Tests: coverage_gate_fails_below_threshold_crate (via guard test below)
  - Measured at main @ a6fe35c4 (report §3): common 99.57, api 98.71,
    trading 97.02, storage 91.35, core 90.38, app 63.43. Floors set 0.07–0.13
    BELOW measured (one decimal, conservative) so normal line-count drift
    doesn't flap: common 99.5, api 98.6, trading 96.9, storage 91.2,
    core 90.2, app 63.3, default 63.0. 100% stays documented as the TARGET;
    floors only ever move UP (ratchet discipline documented in the file).
  - Why in P0: with the gate fixed and thresholds left at 100.0, the
    post-merge "Coverage & Perf" job goes permanently RED (every crate below
    100) — recreating the exact chronic-red incident #1072 just fixed. An
    honest, enforced floor that ratchets up is the no-false-OK alternative.

- [x] Add ratchet guard test that executes the gate against fixtures
  - Files: crates/storage/tests/coverage_gate_guard.rs
  - Tests: coverage_gate_prints_rows_for_absolute_paths,
    coverage_gate_fails_closed_on_zero_matched_files,
    coverage_gate_fails_below_threshold_crate,
    coverage_gate_source_has_no_anchored_match
  - Runs `bash scripts/coverage-gate.sh <fixture>` via std::process::Command
    with temp-dir JSON fixtures (absolute paths); plus a source-scan that the
    anchored `re.match(r'crates/` pattern never returns to the script.

## Design

Single root cause: start-anchored regex vs absolute llvm-cov paths. Fix is
`re.search` + a fail-closed guard on the empty aggregation map. Thresholds move
from fictional 100.0 (never enforced — the gate has been vacuous since its
first absolute-path run) to measured ratchet floors so the fixed gate is GREEN
on day one and tightens monotonically. Enforcement is pinned by a Rust
integration test in `crates/storage/tests/` (repo convention for repo-level
guards) that actually EXECUTES the gate with both bug-shaped and healthy
fixtures, so any regression of either fix fails `cargo test --workspace` in CI.

## Edge Cases

- Relative-path JSON (local runs from repo root): `re.search` matches those too
  — behavior unchanged for the already-working case (pinned by the
  below-threshold fixture using relative paths).
- Windows-style separators: not applicable (CI + dev are Linux/macOS; llvm-cov
  emits `/` paths).
- A crate with `count == 0` lines: existing `pct = 100.0` arm retained.
- Path containing `crates/` twice (e.g. `/home/x/crates/foo/crates/bar/...`):
  `re.search` takes the FIRST occurrence — acceptable; CI checkout root never
  nests a second `crates/` above the real one.
- Fixture portability: guard test writes fixtures to `std::env::temp_dir()`
  with absolute `/home/runner/...`-style filename STRINGS inside the JSON (the
  strings need not exist on disk — the gate only reads the JSON).
- `python3` absent: gate already requires it (GHA runners ship it); guard test
  skips gracefully if `bash`/`python3` are unavailable (same pattern as other
  shell-out guards) — CI runners always have both.

## Failure Modes

- Gate fix wrong → guard test `coverage_gate_prints_rows_for_absolute_paths`
  fails the build (asserts ≥1 per-crate row AND exit 0 on a healthy fixture).
- Someone reverts fail-closed → `coverage_gate_fails_closed_on_zero_matched_files`
  fails the build (asserts exit ≠ 0 on a JSON whose files match no crate).
- Someone re-anchors the regex → source-scan test fails the build.
- Thresholds set ABOVE real coverage by mistake → first post-merge Coverage &
  Perf run goes red loudly (that is the gate working, not a failure mode of
  this change); floors were set 0.07–0.13 below measured to prevent flap.
- Coverage drifts DOWN below a floor → gate fails post-merge CI: intended
  behavior (that is the entire point of P0).

## Test Plan

- `cargo test -p tickvault-storage --test coverage_gate_guard` — 4 new tests.
- Manual evidence in PR body: run the FIXED gate against the real 18.3 MB
  `coverage.json` from the report session — must print 6 per-crate rows
  (PASS at the new floors) and exit 0; and against the synthetic zero-match
  fixture — must exit 1.
- Scoped per testing-scope.md: storage-crate tests only (no `crates/common`
  source change). Pre-push fast gates + banned-pattern + pub-fn guards.

## Rollback

`git revert` of the single squash commit restores the old script, old
thresholds, and deletes the guard test in one step. No data, schema, config
runtime, or deploy surface is touched; nothing else depends on the new
behavior. (Note: rollback restores the VACUOUS gate — only do this if the fixed
gate itself malfunctions, not because it reports red honestly.)

## Observability

- The gate's own stdout in the Coverage & Perf job now ALWAYS shows 6
  per-crate rows with real percentages — the operator can read actual coverage
  in every post-merge run log (previously: zero rows, silence).
- Vacuous-pass condition now produces an explicit red job with the message
  "gate matched 0 files — refusing vacuous pass" instead of green silence.
- No new metrics/alerts: this is CI-side enforcement; the CI job status IS the
  alert channel (GitHub red X + the operator's existing PR/merge monitoring).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Real CI coverage.json (absolute paths), all crates ≥ floors | 6 rows printed, exit 0 |
| 2 | Absolute paths, one crate below its floor | row shows FAIL, exit 1 |
| 3 | JSON with zero `crates/`-matching files | explicit refusal message, exit 1 |
| 4 | Relative-path JSON below threshold (legacy local case) | still FAIL, exit 1 (unchanged) |
| 5 | Anchored-regex regression reintroduced | source-scan guard test fails build |
