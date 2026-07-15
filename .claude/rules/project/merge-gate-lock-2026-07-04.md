# Merge Gate Lock — Nothing Merges Without All Green (Operator Lock 2026-07-04)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §H > `pr-completion-protocol.md` > this file > defaults.
> **Scope:** PERMANENT. Every PR, every merge to `main`, every future Claude/Cowork session.
> **Operator-locked:** 2026-07-04 (verbatim quote below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demand (preserve exactly, do not paraphrase)

**Quote (2026-07-04):**
> "without going fully green how the fuck most of the PRs got merged... if it is merged means it should have the confidence of it went through a deep drill down in depth thorough check"

---

## §1. The incident this lock answers (Verified, 2026-07-04 audit)

| Finding | Evidence |
|---|---|
| Only 4 checks were branch-protection-required (Build & Verify, Security & Audit, Commit Lint, Secret Scan) — the 6 per-crate Test jobs were NOT | PRs #1387 / #1388 / #1382 merged 2026-07-03 while `Test (storage)` was still RUNNING; it FAILED 5–11 seconds after each merge (job log 85045198565: `partition_retention_coverage_guard` panic) |
| Auto-merge armed at PR OPEN, firing the instant the 4 required checks passed | `auto-merge.yml` `gh pr merge --auto --squash` on `pull_request: opened` |
| Coverage gate was post-merge-only (`if: github.event_name == 'push'`) | Main run 28675492296 FAILED post-merge: storage 90.90% < 91.2% floor — the breaching PRs had already merged; main stayed red until #1391 |
| Mutation PR runs ALL died at the 60-min timeout — gated nothing, burned a runner per PR | 4/4 recent runs `cancelled` at 60.3–60.6 min; 8 of 15 recent PRs merged with it in_progress |
| Push-to-main CI was `cancel-in-progress: true` — rapid merges CANCELLED post-merge gates entirely | Cancelled main runs observed 09:46–12:47 IST on 2026-07-03 — those commits' Coverage gate never ran at all |
| The 15-script local hook battery never ran server-side | Only plan-gate + secret-scanner had CI twins; `git push --no-verify` / web edits bypassed the rest |

---

## §2. The rule (one line)

**A PR merges to `main` ONLY when the `All Green` fan-in job in ci.yml — which needs EVERY PR-relevant job (Build & Verify, all 6 Test crates, Security & Audit, Commit Lint, Design-First Wall, Deploy Lint, Coverage & Perf, Repo Guards) — has succeeded, and auto-merge is armed ONLY after that success, never at PR open.**

---

## §3. The mechanical contract (what changed 2026-07-04)

| # | Mechanism | Where |
|---|---|---|
| 1 | Coverage & Perf runs on every PR (was push-only) and feeds All Green | `ci.yml` `coverage-and-perf` (the `if: push` gate deleted) |
| 2 | `All Green` fan-in job: `if: ${{ !cancelled() }}` — runs even when upstream jobs FAIL (so it can report the red verdict) but skips when the whole run was cancelled/superseded by the PR concurrency group (proven live on PR #1392: the superseded PR-open run's All Green stamped a misleading red with every need `cancelled`; the fix leaves the verdict to the superseding run). Fails unless every needed job result == `success` ('skipped' counts as FAILURE on pull_request events; only the PR-only jobs — Commit Lint, Design-First Wall — may legitimately skip, and only on push events). A skipped All Green can never arm auto-merge (`enable-auto-merge` requires result == 'success'), and a cancelled run leaves the other required checks in `cancelled` (merge-blocking) state | `ci.yml` `all-green` |
| 3 | Auto-merge arming moved BEHIND All Green: `enable-auto-merge` job with `needs: all-green`, preserving the PR #1390 same-repo/fork-safety guard verbatim + the draft skip. The old at-open arming in `auto-merge.yml` is retired (the file survives as a manual-dispatch fallback with the same fork guard re-verified via API) | `ci.yml` `enable-auto-merge`, `auto-merge.yml` |
| 4 | Push-to-main CI runs are NEVER cancelled (grouped by SHA); PR runs keep cancel-in-progress | `ci.yml` `concurrency` |
| 5 | Mutation PR trigger REMOVED — scheduled weekly full sweep + push-scoped runs remain; mutation stays a hard-fail gate on the SCHEDULED lane | `mutation.yml` |
| 6 | `Repo Guards` job runs the self-contained source-scan hooks server-side (banned-pattern, data-integrity, O(1)/dedup, boot-symmetry) on every PR + push — closing the `--no-verify` bypass class. Ratchet-baseline guards (test-count, pub-fn-test, financial) stay local-only: their baselines are gitignored, so a CI run would establish a fresh baseline and pass vacuously (false-OK, audit Rule 11) | `ci.yml` `repo-guards` |
| 7 | Weekly full-tree secret scan (every tracked scan-relevant file, not just diffs) | `secret-scan.yml` `secret-scan-full-tree` |
| 8 | Groww QuestDB E2E gains a path-filtered PR lane (same paths as its push lane) — **RETIRED 2026-07-15 with the Groww live feed** (operator directive, this PR's dated note below): its sole target `groww_live_pipeline_e2e.rs` was deleted, so the lane would hard-fail every triggering PR; `groww-e2e.yml` removed, retirement pinned by `crates/storage/tests/coverage_claim_honesty_guard.rs::groww_e2e_lane_retired_with_live_feed` | ~~`groww-e2e.yml`~~ (deleted) |
| 9 | Branch protection on `main` (operator one-time click): required checks = Build & Verify, Security & Audit, Commit Lint, Secret Scan, **All Green**; require branches up to date; do not allow bypassing (enforce for admins) | GitHub Settings → Branches (no API tool available in-session) |

---

## §3.1. 2026-07-04 (same day) — GITHUB_TOKEN bot-merge suppression gap + the post-merge catch-up contract

**The gap (Verified, live on the FIRST bot merge):** §3 row 3 moved auto-merge
arming behind All Green, armed via the workflow's own `GITHUB_TOKEN`
(`gh pr merge --auto --squash` in ci.yml `enable-auto-merge`). PR #1392 then
merged as `github-actions[bot]` (merged_at 2026-07-04T03:06:05Z) — and GitHub
**suppresses push-triggered workflows for events created with `GITHUB_TOKEN`**
(recursion protection). Merge commit `aafa226` therefore landed on `main` with
**ZERO post-merge runs**: no CI (no Coverage & Perf ratchet artifact on main),
no Benchmarks & Perf Gate, no deploy-aws push trigger. Earlier merges fired
post-merge runs only because auto-merge had been armed with the operator's own
token (PR #1391, merged_by SJParthi → push runs fired normally). The repo has
NO PAT secret and the operator wants zero manual steps, so "arm with a
PAT/GitHub-App token" is not the chosen fix.

**The catch-up contract (the fix — cron/dispatch redundancy, the proven house
pattern from `deploy-aws-after-close.yml`):** `workflow_dispatch` and
`repository_dispatch` are the DOCUMENTED exceptions to the GITHUB_TOKEN
suppression rule — a GITHUB_TOKEN-created dispatch DOES fire the workflow.

| # | Mechanism | Where |
|---|---|---|
| 1 | `.github/workflows/postmerge-catchup.yml` — every 30 min (+ manual dispatch): read main HEAD; for each of `ci.yml` and `bench.yml`, list runs filtered by `head_sha=HEAD` (ANY event, ANY status); if ZERO exist → `gh workflow run <wf> --ref main` and log loudly. Idempotent + self-terminating: the dispatched run itself carries `head_sha == HEAD`, so the next firing finds it; a run in any state (queued/in_progress/completed incl. failed) counts as covered — the dispatcher backfills MISSING runs, it never re-runs red ones | `postmerge-catchup.yml` |
| 2 | ci.yml gains a `workflow_dispatch` trigger; a dispatched run on main behaves EXACTLY like a push-to-main run: concurrency groups non-PR runs by SHA (never cancelled), Coverage & Perf runs (no `if:` excludes it — ratchet artifact produced), and All Green tolerates the PR-only jobs (Commit Lint, Design-First Wall) as `skipped` on `workflow_dispatch` exactly as it does on `push` | `ci.yml` `on:` / `concurrency` / `all-green` |
| 3 | bench.yml already had `workflow_dispatch` (and no push-only `if:` conditions) — no change needed | `bench.yml` |
| 4 | deploy-aws is NOT handled by the catch-up dispatcher: it already self-heals via `deploy-aws-after-close.yml` (5 weekday crons + idempotent HEAD-vs-last-deployed-sha compare + `workflow_dispatch` of deploy-aws — a dispatch fires regardless of the suppressed merge push). A weekday bot merge deploys at the next cron slot; a weekend bot merge deploys Monday 08:30 IST. **(SUPERSEDED 2026-07-09:** deploy-aws JOINED the dispatcher's backfill list after both morning crons were scheduler-dropped and the box booted on a stale binary — the dated header in `postmerge-catchup.yml` carries the incident.**)** | `deploy-aws-after-close.yml` + `postmerge-catchup.yml` |
| 5 | **2026-07-14 (PR-C3 rider):** `terraform-apply.yml` joined the dispatcher's backfill list — its push trigger is PATH-FILTERED (`deploy/aws/terraform/**` + its own file), so a bot-merged terraform change was the LAST push-suppressed lane with no dispatcher coverage (its 3 post-close crons are the same scheduler-drop-fragile class as the 2026-07-09 deploy-aws outage). Because the path filter makes zero-runs-for-HEAD the NORMAL state for non-terraform merges, the dispatcher dispatches terraform-apply ONLY when the HEAD commit actually touched the filtered paths (commit-files API; ≤300-file envelope — a larger merge falls back to the post-close crons; fail-safe direction: a missed dispatch defers the apply, never doubles it — the apply is plan-gated + DynamoDB-locked) | `postmerge-catchup.yml` |

**Honest envelope of the catch-up:** detection latency is up to ~30 min (plus
GitHub's scheduler jitter; cron slots CAN be dropped repo-wide as on
2026-07-02 — the next slot covers it, so worst-case latency is bounded only by
the scheduler recovering). The dispatched run is attributed to
`github-actions[bot]` with event `workflow_dispatch` — required-check UI on
the merge COMMIT differs cosmetically from a push run, but every job, gate,
and artifact is identical. The dispatcher cannot backfill SHAs that are no
longer main HEAD (an older suppressed merge that was immediately superseded
by a newer merge is covered transitively — the newer HEAD's full battery runs
against a tree that CONTAINS the older commit).

**What a PR that violates §3.1 looks like (REJECT):** removes
`postmerge-catchup.yml` or its schedule; removes the `workflow_dispatch`
trigger from ci.yml or bench.yml; adds an `if: github.event_name == 'push'`
(or any dispatch-excluding condition) to `coverage-and-perf` or any other
post-merge-relevant ci.yml job; makes All Green fail dispatched main runs on
the PR-only jobs' legitimate skips; or re-points the dispatcher's existence
check at anything weaker than the `head_sha` run filter (the loop-safety
core).

---

## §3.2. 2026-07-05 — auto-merge persists across branch updates (PR #1411 red merge)

**The incident (Verified, 2026-07-05):** PR #1411 (head `1338c26b`) merged at
**14:30:12Z — 4 seconds after the last legacy-required check** (the original
4: Build & Verify, Security & Audit, Commit Lint, Secret Scan) went green,
while **All Green and Coverage & Perf were still in-progress on that head and
later FAILED** on it. The failure was the flaky
`crates/api/tests/tv_api_token_prod_guard.rs` race (3 tests mutating the
process-global `TV_API_TOKEN` env var on parallel test threads; under
llvm-cov instrumentation the interleaving flipped an asserted branch —
`1 target failed: -p tickvault-api --test tv_api_token_prod_guard`, exit
101). The post-merge run on the MERGE COMMIT happened to pass, so `main`
stayed green — luck, not the gate. PRs **#1412 and #1415** merged through
the same gap with green-after-the-fact luck.

**The two-hole chain (why §3 did not stop it):**

| # | Hole | Detail |
|---|---|---|
| 1 | **All Green is still not a GitHub branch-protection required check** | §3 row 9 (the operator's one-time Settings → Branches click adding `All Green` to the required list) has NOT been performed. GitHub therefore considered the PR mergeable on the legacy 4 alone. |
| 2 | **Auto-merge arming is per-PR and SURVIVES branch updates** | GitHub's auto-merge flag, once armed, persists when new commits are pushed to the PR branch. So an arming that was legitimately gated on All Green success for an OLD head (ci.yml `enable-auto-merge`, §3 row 3) stays armed for a NEW head — and the NEW head then merges the instant the legacy-4 required checks pass, without All Green ever succeeding on the head that actually merged. |

**The operative rule until the §3 row 9 click lands (BINDING for every
Claude/Cowork session):**

1. **Do NOT arm auto-merge** (`gh pr merge --auto`, `enable_pr_auto_merge`,
   or any equivalent) on any PR. The ci.yml `enable-auto-merge` job may still
   arm bot-path PRs; agents must not ADD arming on top, and where an agent
   controls the merge, it must disarm/skip arming.
2. **Merge only after verifying `All Green` = `success` on the EXACT final
   head SHA** of the PR (`pull_request_read` head vs the check run's
   `head_sha` — they must match; a green All Green on a superseded head
   counts for NOTHING).
3. After the operator performs the §3 row 9 click (required checks include
   `All Green`, require branches up to date, enforce for admins), this
   operative rule relaxes back to §3's normal auto-merge-behind-All-Green
   flow — the click makes hole 1 impossible and defangs hole 2 (GitHub
   itself will refuse the merge until All Green passes on the final head).

**The flaky-test fix (same PR as this section):**
`tv_api_token_prod_guard.rs` now serializes its env-mutating tests behind a
shared static `Mutex` with a scoped `TokenEnvGuard` (poisoning-safe via
`unwrap_or_else(|e| e.into_inner())`, restores the prior env value on Drop).
Assertions unchanged; verified 20/20 consecutive green runs locally. The
gate hole and the flake are INDEPENDENT failures — fixing the flake does not
excuse the gap, and closing the gap would not have fixed the flake.

**What a PR that violates §3.2 looks like (REJECT):** re-introduces agent-side
auto-merge arming before the branch-protection click is confirmed; merges on
a stale-head All Green; treats "post-merge run passed" as retroactive
justification for a red pre-merge gate.

---

## §4. The honest envelope (mandatory per operator-charter §F — no illusion)

- **Pre-merge gate = the fast battery** (build, clippy, the full 6-crate test suites incl. DHAT + proptest, security audit, coverage ratchet, source-scan guards, plan gate, secret diff-scan). Warm-cache PR wall-clock moves from ~4 min to ~8–10 min (coverage is the new critical path; ~20–25 min on a cold cache).
- **Mutation / fuzz / ASan / TSan / cargo-careful stay SCHEDULED, not pre-merge — by physics, not laziness.** The full mutation sweep over core/trading/common is an ~18-hour-class job (hundreds of mutants, each a build+test cycle); fuzz is 1h/target by design; sanitizers rebuild the workspace with `-Z build-std` single-threaded (hours). None of these can complete inside a per-PR runner budget. They remain hard-fail on their weekly/scheduled lanes with auto-filed issues. Claiming they gate PRs (as mutation.yml's old header did) was a false-OK and is exactly what this lock forbids.
- **`#[ignore]`d QuestDB/Docker tests still skip green inside the PR Test jobs** — the Groww E2E PR lane covered its slice non-vacuously until its 2026-07-15 retirement with the Groww live feed (no live pipeline remains to E2E); the chaos suite remains scheduled (and is separately known-broken; not fixed by this lock).
- **Branch protection itself is a GitHub setting** — until the operator flips §3 row 9, the bot path is safe (arming waits for All Green) but an admin manual merge can still bypass (the #1390 58-second owner merge class). The click is the last brick.

---

## §5. What a PR that violates this lock looks like (REJECT)

- Removes, renames, or weakens the `all-green` job, or removes ANY job from its `needs:` list without a dated operator quote.
- Adds a new PR-relevant job to ci.yml WITHOUT adding it to `all-green`'s `needs:` list (the job would run but not gate merges — silent regression of the choke point).
- Re-introduces at-PR-open auto-merge arming (in auto-merge.yml, ci.yml, or any new workflow), or arms auto-merge from any job that does not `needs: all-green`.
- Weakens the PR #1390 same-repo/fork-safety guard on any arming path (`head.repo.full_name == github.repository` is non-negotiable).
- Restores `if: github.event_name == 'push'` (or any PR-excluding condition) on `coverage-and-perf`.
- Restores `cancel-in-progress: true` for push-to-main runs of ci.yml (or re-groups them so rapid merges cancel each other's post-merge gates).
- Re-adds the mutation `pull_request` trigger as-is (without an `--in-diff` redesign + realistic timeout + dated operator quote).
- Removes the `Repo Guards` job or any of its four source-scan steps, or converts a guard to `continue-on-error`.
- Removes the weekly full-tree secret scan. *(The Groww E2E PR lane half of this row is SUPERSEDED 2026-07-15 — the operator's Groww live-feed retirement directive deleted the lane's test target, so the lane itself was retired in the same PR; re-adding a Groww live E2E lane needs a fresh dated quote here first.)*
- Treats `skipped` as success in the all-green evaluation for anything other than the PR-only jobs on push events.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land.

---

## §6. Auto-driver / Insta-reel explanation

> Sir, imagine the juice shop had ten quality checks — but the delivery boy was told "leave the moment the FIRST FOUR pass." So bottles went out while the last six checks were still running, and three times the taste test FAILED seconds after the bottle had already left. The new rule: ONE final inspector called "All Green" stands at the door. He does nothing himself — he just looks at ALL ten check sheets and only stamps the bottle when every single one says PASS. The delivery boy is not even TOLD about the bottle until All Green stamps it. And the overnight deep tests (the 18-hour lab analysis) still happen every week — we just stopped PRETENDING they happened per-bottle when they physically cannot.

---

## §7. Trigger (auto-loaded paths)

Always loaded. Reinforced on any session that:
- Edits `.github/workflows/ci.yml`, `auto-merge.yml`, `mutation.yml`, `secret-scan.yml`, or `groww-e2e.yml`
- Edits any file containing `all-green`, `All Green`, `enable-auto-merge`, `gh pr merge --auto`, or `cancel-in-progress`
- Opens, arms, or merges any PR
