# Implementation Plan: Post-Merge Catch-up Dispatcher (bot-merge suppression gap)

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — 2026-07-04 standing automation demand (zero manual steps; every merge to main must get its full post-merge battery). Guarantee matrices: cross-referenced per `.claude/rules/project/per-wave-guarantee-matrix.md` (workflow/docs-only change — no Rust code, no hot path, no new tick-drop path).

## Plan Items

- [x] Add `workflow_dispatch` trigger to ci.yml; dispatched main runs behave like push runs (concurrency by SHA never cancelled; All Green tolerates PR-only-job skips on dispatch; Coverage & Perf unchanged — no `if:` excludes dispatch)
  - Files: .github/workflows/ci.yml
  - Tests: PyYAML parse of all workflows; `r17_ci_test_matrix_runs_integration_tests_not_just_lib` (github_workflow_guard.rs) stays green; coverage_claim_honesty_guard.rs stays green
- [x] Confirm bench.yml already has `workflow_dispatch` and no push-only conditions (no change)
  - Files: .github/workflows/bench.yml (read-only confirmation)
  - Tests: PyYAML parse
- [x] New `.github/workflows/postmerge-catchup.yml` — every-30-min cron + manual dispatch; for ci.yml + bench.yml, if main HEAD has ZERO runs (any event/status, `head_sha` filter) dispatch that workflow on main; idempotent + self-terminating
  - Files: .github/workflows/postmerge-catchup.yml
  - Tests: PyYAML parse; loop-safety reasoning pinned in §3.1 of the rule file
- [x] Document the suppression gap + catch-up contract as §3.1 in the merge-gate lock rule file
  - Files: .claude/rules/project/merge-gate-lock-2026-07-04.md
  - Tests: n/a (docs); REJECT list added for future PRs
- [x] deploy-aws: confirm existing after-close cron redundancy self-heals bot merges (no change)
  - Files: .github/workflows/deploy-aws-after-close.yml (read-only confirmation)
  - Tests: `r11..r13` deploy-aws ratchets in github_workflow_guard.rs stay green

## Design

PR #1392 merged as `github-actions[bot]` (ci.yml `enable-auto-merge` arms native
auto-merge with `GITHUB_TOKEN`). GitHub suppresses push-triggered workflows for
GITHUB_TOKEN-created events, so merge commit `aafa226` got ZERO post-merge runs
(no CI/coverage ratchet, no bench, no deploy push trigger). `workflow_dispatch`
/ `repository_dispatch` are the documented exceptions to that suppression, so a
scheduled dispatcher (`postmerge-catchup.yml`, every 30 min) checks whether main
HEAD has any run of ci.yml / bench.yml (GitHub runs API, `head_sha` filter, any
event, any status) and dispatches the missing workflow on main. ci.yml gains a
`workflow_dispatch` trigger whose main runs are semantically identical to push
runs (SHA-grouped never-cancel concurrency; All Green skip-tolerance for the
PR-only Commit Lint + Design-First Wall jobs extended from `push` to
`{push, workflow_dispatch}`; Coverage & Perf has no event gate). bench.yml
already had `workflow_dispatch`. deploy-aws is excluded — it already self-heals
via `deploy-aws-after-close.yml` (weekday crons + idempotent sha compare +
workflow_dispatch of deploy-aws).

## Edge Cases

- Dispatched run itself: has `head_sha == main HEAD` + event `workflow_dispatch`
  → the next scheduled check counts it → no re-dispatch (self-terminating; no
  infinite loop).
- A run already exists but FAILED/CANCELLED for the SHA → counted as covered;
  the dispatcher never re-runs red runs (backfill-missing only, per spec).
- Rapid successive bot merges: only the current main HEAD is checked; an older
  suppressed SHA superseded within the 30-min window is covered transitively by
  the newer HEAD's full-battery run (its tree contains the older commit).
- Human/PAT merge (push run fires normally) → `total_count >= 1` → no dispatch.
- Two dispatcher firings racing: `concurrency: postmerge-catchup` +
  `cancel-in-progress: false` serializes them; even if both dispatched, ci.yml's
  SHA-grouped concurrency queues rather than duplicates work destructively.
- Manual `workflow_dispatch` of the dispatcher: same idempotent path.

## Failure Modes

- GitHub cron scheduler drops slots (observed repo-wide 2026-07-02): the next
  30-min slot covers it; worst-case latency bounded by scheduler recovery, and
  the dispatcher is manually dispatchable. Honest envelope: cron redundancy
  reduces but cannot eliminate scheduler risk.
- `gh api` / `gh workflow run` failure: `set -euo pipefail` fails the job
  loudly (red run in the Actions tab + `::warning` annotations when dispatching).
- GITHUB_TOKEN loses `actions: write`: dispatch returns 403 → job fails loudly.
- ci.yml dispatched run red: identical semantics to a red push run — surfaced
  on main's commit status; nothing new to handle.

## Test Plan

- PyYAML-parse every file under `.github/workflows/` (syntax gate).
- Run the workflow-pinning ratchet tests locally:
  `cargo test -p tickvault-common --test github_workflow_guard` and
  `cargo test -p tickvault-storage --test coverage_claim_honesty_guard`
  (plus the other storage guards that grep workflows:
  `deploy_no_disable_on_failure_guard`).
- Post-merge live proof: the first scheduled firing after this PR merges must
  backfill the then-current main HEAD if it lacks runs (the PR body notes this;
  a same-session manual dispatch of ci.yml on main is attempted via the GitHub
  MCP `actions_run_trigger` tool to backfill today immediately).

## Rollback

`git revert` of the single squash commit restores the previous state exactly:
delete `postmerge-catchup.yml`, drop the ci.yml `workflow_dispatch` trigger +
All Green/concurrency dispatch arms, drop the rule-file §3.1 and this plan.
No schema, no code, no data migration — pure workflow/docs; zero runtime
surface in the trading app.

## Observability

- The dispatcher writes a `$GITHUB_STEP_SUMMARY` (main HEAD + dispatch count)
  and emits `::warning::` annotations on every backfill dispatch — visible in
  the Actions tab per firing.
- A backfilled SHA becomes visible exactly where the gap was: the run list for
  ci.yml / bench.yml filtered by that `head_sha` goes from 0 to ≥1, and the
  Coverage & Perf ratchet artifact reappears on main.
- Regression detection: §3.1 REJECT list in
  `.claude/rules/project/merge-gate-lock-2026-07-04.md` (auto-loaded every
  session) names the violating diffs (removing the trigger, re-adding a
  push-only gate on coverage, weakening the head_sha existence check).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Bot (GITHUB_TOKEN) auto-merge lands on main | Within ~30 min, ci.yml + bench.yml each get a dispatched run on the merge SHA with the full battery |
| 2 | Human/PAT merge lands on main | Push runs fire natively; dispatcher sees runs exist; no dispatch |
| 3 | Dispatched run completes (green or red) | Next firing counts it; no re-dispatch loop |
| 4 | Cron slot dropped by GitHub | Next slot (or manual dispatch) backfills; bounded latency |
| 5 | Bot merge superseded by newer merge before backfill | Newer HEAD's battery covers the tree transitively |
