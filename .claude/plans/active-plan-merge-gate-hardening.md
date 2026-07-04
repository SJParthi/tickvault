# Implementation Plan: Merge-Gate Hardening — Nothing Merges Without All Green

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — verbatim demand 2026-07-04, this session: "without going fully green how the fuck most of the PRs got merged... if it is merged means it should have the confidence of it went through a deep drill down in depth thorough check"

> **Guarantee matrices:** this plan cross-references the mandatory 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` and
> `operator-charter-forever.md` §C. This is a workflow/config/docs-only change —
> no `crates/*/src` file is touched, no hot path, no new tick-drop path, no DEDUP
> key, no new pub fn. The applicable rows are: 100% code checks (the entire point
> — guards move server-side), 100% scenarios (edge cases below), 100% code review
> (this PR), 100% extreme check (the merge-gate-lock rule file's REJECT section is
> the ratchet); all Rust-code rows are N/A — no Rust code changed.

## Design

Single choke point: a new `all-green` fan-in job in `ci.yml` with `needs:` on
every PR-relevant job (commit-lint, design-first-wall, deploy-lint,
build-and-verify, the 6-crate test matrix, security-and-audit,
coverage-and-perf, repo-guards) and `if: always()`. It fails unless every
needed result is `success` — `skipped` counts as failure on pull_request
events; only the PR-only jobs (commit-lint, design-first-wall) may skip, and
only on push events. Auto-merge arming moves from `auto-merge.yml`'s at-open
trigger to a `ci.yml` `enable-auto-merge` job with `needs: all-green`,
preserving the PR #1390 same-repo/fork-safety guard and draft skip verbatim.
`coverage-and-perf` loses its `if: github.event_name == 'push'` gate so the
coverage ratchet blocks BEFORE merge. Push-to-main CI runs are grouped by SHA
with `cancel-in-progress` false-for-push so rapid merges can no longer cancel
post-merge gates. The dead mutation PR lane (every run killed at the 60-min
ceiling; full sweep is ~18h-class) is removed; weekly + push-scoped mutation
stays hard-fail. A new `repo-guards` job runs the four self-contained
source-scan hooks (banned-pattern, data-integrity, dedup-latency,
boot-symmetry) server-side. secret-scan.yml gains a weekly full-tree sweep;
groww-e2e.yml gains a path-filtered PR lane. CLAUDE.md's CI/CD section is
corrected (incl. the stale "DHAT is post-merge-only" claim) and the operator
lock is recorded in `.claude/rules/project/merge-gate-lock-2026-07-04.md`.
Branch protection (required checks += All Green, strict up-to-date, enforce
admins) is a one-time operator click documented in the PR body — no API tool
for it exists in this session.

## Edge Cases

- **Push-to-main event:** all-green also runs on push; commit-lint +
  design-first-wall legitimately skip there — the evaluator allowlists exactly
  those two job ids, only on `push` events.
- **Matrix aggregation:** `needs.test.result` aggregates all 6 crate legs; any
  leg failure (or cancellation) fails all-green.
- **Cancelled upstream:** result `cancelled` is treated as failure — a
  superseded/cancelled run can never fake green.
- **Unexpected skip on a PR:** `skipped` fails all-green on pull_request
  events, so a future `if:`/path-filter added to an upstream job cannot
  silently un-gate it.
- **Fork PR:** enable-auto-merge re-checks `head.repo.full_name ==
  github.repository` (PR #1390 guard) — a fork branch named `claude/*` is
  never armed; the manual-dispatch fallback in auto-merge.yml re-verifies the
  same via API before arming.
- **Draft PR:** never armed (`draft != true` guard preserved).
- **Coverage floor red on main:** PR #1391 (storage floor fix) merged BEFORE
  this plan's PR opens — sequenced explicitly so making coverage PR-blocking
  cannot deadlock every PR.
- **Ratchet-baseline hooks in CI:** deliberately EXCLUDED from repo-guards —
  their baselines are gitignored, so a fresh checkout would establish a new
  baseline and pass vacuously (false-OK, audit Rule 11).
- **This PR's own CI:** pull_request-triggered workflows run from the PR merge
  ref, so this PR is itself gated by the new all-green + post-All-Green arming.
- **Path-filtered lanes:** groww-e2e PR lane and mutation are NOT in
  all-green's needs (cross-workflow needs are impossible; path-filtered checks
  cannot be required without hanging non-matching PRs).

## Failure Modes

- **all-green evaluator bug (false fail):** merges block, nothing unsafe
  merges — fail-closed by construction; fix-forward on the evaluator.
- **all-green evaluator bug (false pass):** bounded by branch protection still
  requiring the original 4 checks; the REJECT section of the merge-gate-lock
  rule file makes any weakening reviewable.
- **Coverage job flaky/slow on cold cache (~20-25 min):** PR merge waits; the
  45-min job timeout bounds it; retry via re-run, never via gate removal.
- **enable-auto-merge token lacks permission:** job fails visibly; the PR is
  simply not auto-armed — operator can arm manually (auto-merge.yml dispatch
  fallback) after checks are green; no unsafe path opens.
- **Full-tree secret scan exceeds runner budget:** 30-min timeout with a
  measured ~3-7 min expected runtime on the 670 scan-relevant files; a timeout
  fails the weekly run loudly (never blocks PRs).
- **Operator never clicks branch protection:** the bot path is already safe
  (arming waits for All Green); the residual risk is an admin manual merge —
  documented honestly in the PR body's operator-action section.

## Test Plan

- Parse every workflow file with PyYAML (`yaml.safe_load` over
  `.github/workflows/*.yml`) — must all load cleanly.
- Run each repo-guards script locally from the clean tree and confirm exit 0:
  `banned-pattern-scanner.sh`, `data-integrity-guard.sh`,
  `dedup-latency-scanner.sh`, `boot-symmetry-guard.sh` (done 2026-07-04, all
  exit 0).
- Run the pre-filtered full-tree secret scan locally and confirm exit 0.
- Cross-check the all-green `needs:` list against the exact job ids present in
  ci.yml (`commit-lint`, `design-first-wall`, `deploy-lint`,
  `build-and-verify`, `test`, `security-and-audit`, `coverage-and-perf`,
  `repo-guards`).
- Unit-test the all-green evaluator logic locally (python snippet with
  success/skipped/failure/cancelled permutations for both event types).
- Live proof: this PR's own check run must show All Green waiting on the full
  suite and Enable Auto-Merge running only after it.

## Rollback

Every change is a plain-text workflow/docs edit: `git revert` of the squash
commit restores the previous merge-gate behavior in one commit (at-open
arming, push-only coverage, mutation PR lane, cancel-in-progress on main).
Branch protection changes are reverted by unchecking "All Green" in Settings →
Branches. No data migration, no schema, no binary, no runtime state is
involved. Partial rollback is also safe per-file (each workflow is
independent).

## Observability

- The All Green check itself is the operator-visible signal on every PR (one
  named check that is red until the whole suite is green).
- `::error::` annotations name every non-success needed job on failure
  (`all-green FAILED — non-success needed jobs: ...`).
- The GitHub Checks UI on each PR shows Enable Auto-Merge strictly after All
  Green in the dependency graph — arming order is auditable per PR.
- Post-merge main runs are no longer cancellable, so every main commit has a
  complete CI record (the audit's "cancelled main runs" class disappears).
- Weekly full-tree secret scan and weekly mutation sweep report via their
  workflow runs + existing auto-filed issues on failure.
- The merge-gate-lock rule file (§5 REJECT list) is the documented ratchet a
  reviewer applies to any future weakening PR.

## Plan Items

- [x] ci.yml: coverage-and-perf pre-merge (drop `if: push`)
  - Files: .github/workflows/ci.yml
  - Tests: PyYAML parse; live PR run shows Coverage & Perf executing on the PR
- [x] ci.yml: All Green fan-in job over every PR-relevant job
  - Files: .github/workflows/ci.yml
  - Tests: needs-list cross-check vs job ids; evaluator permutation test
- [x] ci.yml: enable-auto-merge job behind All Green (fork guard preserved); auto-merge.yml at-open arming retired (manual-dispatch fallback kept)
  - Files: .github/workflows/ci.yml, .github/workflows/auto-merge.yml
  - Tests: PyYAML parse; live PR shows arming after All Green only
- [x] ci.yml: push-to-main runs never cancel-in-progress (SHA-grouped)
  - Files: .github/workflows/ci.yml
  - Tests: PyYAML parse; expression review
- [x] mutation.yml: remove dead PR lane; keep weekly/push/dispatch; honest ~18h envelope comment; no cancel-in-progress
  - Files: .github/workflows/mutation.yml
  - Tests: PyYAML parse
- [x] ci.yml: Repo Guards job (banned-pattern, data-integrity, dedup-latency, boot-symmetry) wired into All Green
  - Files: .github/workflows/ci.yml
  - Tests: each script run locally from clean tree, exit 0 (2026-07-04)
- [x] secret-scan.yml: weekly full-tree sweep (+dispatch), diff lane gated to push/PR
  - Files: .github/workflows/secret-scan.yml
  - Tests: pre-filtered full-tree scan run locally, exit 0
- [x] groww-e2e.yml: pull_request lane with the same path filter; ref-scoped concurrency
  - Files: .github/workflows/groww-e2e.yml
  - Tests: PyYAML parse
- [x] Branch protection: no API tool exists — exact click-by-click operator instructions in the PR body
  - Files: (PR body)
  - Tests: N/A — GitHub settings
- [x] Docs: CLAUDE.md CI/CD section corrected (incl. stale DHAT claim); new rule file merge-gate-lock-2026-07-04.md
  - Files: CLAUDE.md, .claude/rules/project/merge-gate-lock-2026-07-04.md
  - Tests: N/A — docs
- [x] This plan file (6 gate sections, APPROVED)
  - Files: .claude/plans/active-plan-merge-gate-hardening.md
  - Tests: plan-gate section check

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | PR with red Test (storage), 4 legacy required checks green | All Green red → auto-merge never armed → no merge |
| 2 | PR with coverage floor breach | Coverage & Perf red on the PR → All Green red → no merge |
| 3 | Two PRs merge back-to-back into main | Both main push runs complete (SHA-grouped, never cancelled) |
| 4 | Fork PR from branch `claude/evil` goes fully green | enable-auto-merge `if:` fails the same-repo guard → never armed |
| 5 | Draft PR goes fully green | Not armed (draft guard) |
| 6 | Push-to-main event | commit-lint + design-first-wall skip; All Green still passes (allowlisted skips) |
| 7 | Future job added to ci.yml but not to all-green needs | REJECT per merge-gate-lock §5 (review ratchet) |
| 8 | `--no-verify` local push with a banned pattern | Repo Guards red server-side → All Green red → no merge |
