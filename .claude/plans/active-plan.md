# Implementation Plan: CI-enforce the design-first wall

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban — chose "CI-enforce the plan wall".
**Crate(s) touched:** none (CI workflow only: `.github/workflows/ci.yml`)

## Context

The design-first wall (`.claude/hooks/plan-gate.sh`) blocks an implementation
push (`crates/*/src/**.rs`) that has no APPROVED plan referencing the changed
crate. Today it runs ONLY in the local pre-push hooks
(`.claude/hooks/pre-push-gate.sh` + `scripts/git-hooks/pre-push`), so a push
that bypasses local hooks (`--no-verify`, a web edit, a fork) escapes it. This
item runs the SAME hook as a GitHub CI gate on every PR.

## Plan Items

- [x] Item 1 — Add a `design-first-wall` job to `ci.yml` (pull_request only)
  - Files: `.github/workflows/ci.yml`
  - Tests: `bash .claude/hooks/plan-gate.sh` already self-tested by
    `.claude/hooks/plan-gate.selftest.sh` (11 cases); CI step re-runs the gate

## Design

1. New job `design-first-wall`, `if: github.event_name == 'pull_request'`,
   `runs-on: ubuntu-latest`, mirrors the `commit-lint` job shape.
2. `actions/checkout@v6` with `fetch-depth: 0` so the base commit is present.
3. Run `bash .claude/hooks/plan-gate.sh "${{ github.workspace }}"` with
   `PLAN_GATE_BASE=${{ github.event.pull_request.base.sha }}` — the hook's
   documented env override (L6), giving the exact PR base for the
   `${BASE}...HEAD` diff (no reliance on `origin/main` ref resolution).
   `CLAUDE_PROJECT_DIR=${{ github.workspace }}`.
4. Exit non-zero (gate's exit 2) fails the job → the PR check goes red.

## Edge Cases

- **Docs/CI-only PR (incl. THIS one)** → no `crates/*/src/**.rs` change → gate
  PASSES (so this PR is self-consistent; it does not block itself).
- **`PLAN-EXEMPT:` commit** → gate honours it (≤1 impl file) → PASS.
- **Plan archived before merge** → gate would BLOCK (no active plan). The
  current Claude workflow commits plan + code together and archives only
  AFTER merge, so the plan is present on the PR branch — compatible. Documented
  so a future archive-before-merge flow knows to use `PLAN-EXEMPT:` or keep the
  plan until merge.

## Failure Modes

- The job only READS git + runs the hook; it cannot mutate the repo. Worst case
  is a false RED, recoverable by adding the plan or a `PLAN-EXEMPT:` line.
- Not yet a REQUIRED check (branch protection is an operator UI setting) — the
  job runs + reports; the operator marks it required to make it merge-blocking.
  Stated honestly in the PR.

## Test Plan

- `bash .claude/hooks/plan-gate.sh "$PWD"` locally (this PR = no impl src) → exit 0.
- `PLAN_GATE_BASE=origin/main bash .claude/hooks/plan-gate.sh "$PWD"` → exit 0.
- `bash .claude/hooks/plan-gate.selftest.sh` → 11/11 pass (gate logic unchanged).
- YAML sanity: the job parses (no tab/indent errors).

## Rollback

Delete the `design-first-wall` job from `ci.yml`. No code/schema impact; the
local pre-push hooks still enforce the wall.

## Observability

The PR Checks tab gains a "Design-First Wall" check — green/red per PR. No new
metric (CI-native signal).

## Per-Item Guarantee Matrix (cross-reference)

Bound by the 15-row guarantee matrix + 7-row resilience matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`.

**Honest envelope:** "100% inside the tested envelope, with ratcheted regression
coverage" = the `plan-gate.selftest.sh` 11-case suite (the gate logic this CI
job invokes). This adds a CI surface for an EXISTING, tested gate; it does not
change the gate logic. It becomes merge-blocking only once the operator marks
the check required in branch protection — stated, not assumed.
