# Implementation Plan: Local Window Enablement — Telegram source badge + one-way local-runtime branch

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — verbatim quotes below, 2026-07-04, this session

> **Operator quotes (2026-07-04, verbatim — the authorization for this plan):**
> 1. Local-only branch: *"we need to maintain the branch something like this is
>    purely for local and it should run only on local... whatever the changes or
>    codes or functionalities changes also everything should be merged entirely
>    from all different branches to this branch alone"* — a permanent branch
>    that receives one-way merges FROM main and NEVER merges INTO main.
> 2. Telegram source tag: *"in telegram when it displays message then it should
>    clearly tell whether it is from local or aws"*.

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (cold-path notification + CI-workflow change; no hot-path delta, no new
> tick-drop path, no DEDUP/table change).

## Plan Items

- [x] Item 1 — Telegram runtime source badge (💻 LOCAL / ☁️ AWS) after the
      severity tag on every outgoing Telegram message, ONE implementation
      point (`telegram_message_prefix`), zero hot-path cost (notification
      path is cold).
  - Files: crates/core/src/notification/source_badge.rs (new),
    crates/core/src/notification/service.rs,
    crates/core/src/notification/mod.rs
  - Tests: test_classify_runtime_source_systemd_managed_is_aws,
    test_classify_runtime_source_not_systemd_managed_is_local,
    test_badge_local_is_laptop_emoji_plus_word,
    test_badge_aws_is_cloud_emoji_plus_word,
    test_badges_are_distinct_and_short,
    test_badge_never_leaks_env_details,
    test_runtime_source_is_stable_across_calls,
    test_telegram_message_prefix_starts_with_severity_tag,
    test_telegram_message_prefix_carries_source_badge_after_tag,
    test_telegram_message_prefix_under_cargo_test_is_local

- [x] Item 2 — Telegram style rule file dated note about the badge.
  - Files: .claude/plans/friday-may-15-mega/topic-telegram-message-style-rules.md
  - Tests: N/A — docs only

- [x] Item 3 — `local-branch-sync.yml`: on every push to main (+ manual
      dispatch), merge main INTO `local-runtime` if the branch exists;
      idempotent no-op when up to date or branch absent; on conflict abort,
      open/update the single "local-runtime sync conflict" tracking issue,
      exit loudly.
  - Files: .github/workflows/local-branch-sync.yml (new)
  - Tests: PyYAML parse of all workflows (validation step); github_workflow_guard suite green

- [x] Item 4 — Block-guard: ci.yml `local-runtime-block` job FAILS any PR
      whose head branch is `local-runtime` (or `local-runtime/*`) so the
      one-way contract is merge-blocking; wired into the All Green needs
      list + PR_ONLY_JOBS skip-tolerance set.
  - Files: .github/workflows/ci.yml
  - Tests: r17_ci_test_matrix_runs_integration_tests_not_just_lib (unchanged, still green);
    PyYAML parse; All Green needs-list cross-check (validation step)

- [x] Item 5 — This plan file (Status: APPROVED, 6 sections filled).
  - Files: .claude/plans/active-plan-local-window.md
  - Tests: bash .claude/hooks/plan-gate.sh + per-item-guarantee-check.sh pass

- [x] Item 6 — (pushed directly, NO PR — never merges to main) the
      `local-runtime` branch itself, created FROM origin/main, carrying the
      local-only overlay: config/local.toml `[feeds]` dhan+groww ON
      (dry_run stays true; window runs while AWS is off, so Dhan-on is
      safe), `deploy/docker/docker-compose.local.yml` QuestDB memory bump
      via the existing `mem_limit` pattern (no image/version changes, no
      new services), `README-LOCAL.md` Mon–Wed runbook at repo root.
  - Files: (on branch local-runtime only) config/local.toml,
    deploy/docker/docker-compose.local.yml, README-LOCAL.md
  - Tests: N/A — branch-local overlay; cargo config tests remain green on that branch

## Design

Two independent mechanisms, one goal — the operator can run the system on
his Mac during the local window without ever contaminating main:

1. **Telegram source badge.** Every Telegram message's first line becomes
   `<severity tag> <badge> <body>`, badge ∈ {`💻 LOCAL`, `☁️ AWS`}. The
   discriminating signal is systemd management (`NOTIFY_SOCKET` present at
   process start): `TV_ENVIRONMENT` cannot distinguish the runtimes because
   it DEFAULTS to `prod` when unset (single real env, operator 2026-06-30),
   while the only systemd deployment of the binary is the AWS prod unit
   (`deploy/systemd/tickvault.service`) — the same signal
   `infra::notify_systemd_ready` already keys on. No new env var is
   invented. The probe runs ONCE (OnceLock); both dispatch paths (immediate
   `notify` bypass + coalesced `deliver_summaries`) build their prefix via
   the single `telegram_message_prefix` helper, so the badge can never
   drift between paths. The SNS SMS path reuses the same prefixed message.

2. **One-way `local-runtime` branch machinery.** `local-branch-sync.yml`
   merges main into `local-runtime` on every main push (conventional merge
   message `chore(local): merge main into local-runtime`), no-ops when the
   branch is absent or already up to date, and on conflict aborts + files a
   single tracking issue for the operator to resolve on his Mac. The ci.yml
   `local-runtime-block` job (in All Green's needs list) fails any PR from
   `local-runtime`/`local-runtime/*` into main, making the one-way contract
   merge-blocking. Pushes by GITHUB_TOKEN to local-runtime trigger nothing
   else (leaf branch + GITHUB_TOKEN suppression) — no workflow recursion.

## Edge Cases

- `local-runtime` branch does not exist yet → sync workflow logs + exits 0
  (no-op); nothing is created implicitly.
- `local-runtime` already contains main HEAD (`merge-base --is-ancestor`)
  → idempotent no-op; repeated dispatches are safe.
- Conflicting sync while a tracking issue is already open → the existing
  issue gets a comment (never a duplicate open issue).
- A push to local-runtime racing the sync push → the robot's push fails,
  run goes red, the next main push / manual dispatch retries; the
  `local-branch-sync` concurrency group serialises overlapping runs.
- PR head branch named `local-runtime/foo` → guard fails it (prefix match).
- Push-to-main / workflow_dispatch CI runs → `local-runtime-block` is
  PR-only and legitimately skipped; All Green tolerates the skip via
  PR_ONLY_JOBS exactly like commit-lint / design-first-wall.
- Badge under `cargo test` / CI → resolves LOCAL (not systemd-managed);
  badge value is cached per process, so it can never flip mid-session.
- Operator runs the binary under systemd on a non-AWS host → would badge
  AWS. Documented honest envelope (exactly one systemd unit exists today,
  on the AWS box); guard keys on process-management reality, not wishes.

## Failure Modes

- Sync merge conflict → NEVER auto-resolved: abort, tracking issue
  ("local-runtime sync conflict") with the conflicting file list, red run.
  Operator resolves on the Mac and pushes; next sync is clean.
- gh issue create/comment fails (API outage) → the run is already red from
  the `::error::` + exit 1; the conflict is visible in the run log.
- Renamed head branch bypasses the block-guard → honest envelope: the
  guard keys on the head branch NAME; a rename is a deliberate operator
  act, not the accident class this blocks. Documented in the job comment.
- Badge misclassification can only happen if NOTIFY_SOCKET appears/vanishes
  contrary to the deployment model — the pure classifier + the systemd-unit
  lockstep comment pin the contract; no trading behaviour depends on it
  (display-only string on a cold path).
- Telegram delivery failures are UNCHANGED (existing retry + TELEGRAM-01
  accounting); the badge adds ~9 chars, far inside the 3800-char chunk
  ceiling.

## Test Plan

- `cargo test -p tickvault-core --lib notification` — 294 tests green
  (includes the 10 new badge/prefix tests listed in Items 1).
- `cargo fmt --check` + `cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf`
  (the CI clippy command) — clean.
- PyYAML parse of every file under `.github/workflows/` — all valid.
- Workflow ratchets: `cargo test -p tickvault-common --test github_workflow_guard`
  + `cargo test -p tickvault-storage --test coverage_claim_honesty_guard` — green.
- All Green needs-list cross-check: every PR-relevant ci.yml job appears in
  `all-green.needs`; `local-runtime-block` added to both `needs` and
  PR_ONLY_JOBS (validated by script in the PR run).
- `bash .claude/hooks/banned-pattern-scanner.sh` — clean.

## Rollback

- Badge: revert the service.rs prefix helper call sites to
  `severity.tag()` and delete `source_badge.rs` + the mod.rs line — a
  2-file revert with no data/schema impact (display-only).
- Branch machinery: delete `.github/workflows/local-branch-sync.yml` and
  remove the `local-runtime-block` job + its `all-green` needs entry +
  PR_ONLY_JOBS entry (one commit). Note merge-gate lock §5: removing a job
  from All Green's needs requires a dated operator quote — rollback of this
  feature IS such a change and must cite this plan's operator quotes.
- `local-runtime` branch: delete the remote branch; nothing on main
  references it (the sync workflow no-ops when it is absent).

## Observability

- The badge itself IS the observability deliverable: every Telegram now
  self-identifies its source machine (💻 LOCAL / ☁️ AWS) at a glance.
- Sync robot: each run logs no-op/merged/conflict verdicts; conflicts page
  via the red workflow run + the single "local-runtime sync conflict"
  GitHub issue (edge-style: comment-updates, never duplicate issues).
- Block-guard: a rejected PR shows a red "Local-Runtime Branch Guard"
  check with a plain-English `::error::` explaining the one-way contract;
  All Green stays red so auto-merge can never arm.
- No new metrics/alerts: no new failure mode inside the app runtime (the
  badge is a pure string on the existing cold notification path); CI-side
  failures surface through the existing red-run + required-check surfaces.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Critical alert from the Mac during the local window | First line `🚨 [CRITICAL] 💻 LOCAL ...` |
| 2 | Same alert from the AWS box | First line `🚨 [CRITICAL] ☁️ AWS ...` |
| 3 | Coalesced Low-severity summary drain | Prefix built by the SAME helper → badge present |
| 4 | Push to main with local-runtime existing + mergeable | Robot merges + pushes `chore(local): merge main into local-runtime` |
| 5 | Push to main, local-runtime absent | Robot no-ops, green run |
| 6 | Push to main, merge conflicts | Robot aborts, files/updates the tracking issue, red run |
| 7 | PR opened from local-runtime → main | `Local-Runtime Branch Guard` fails → All Green red → no merge |
| 8 | Push-to-main CI run | Guard skipped (PR-only), All Green tolerates via PR_ONLY_JOBS |
