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
| 2 | `All Green` fan-in job: `if: always()`, fails unless every needed job result == `success` ('skipped' counts as FAILURE on pull_request events; only the PR-only jobs — Commit Lint, Design-First Wall — may legitimately skip, and only on push events) | `ci.yml` `all-green` |
| 3 | Auto-merge arming moved BEHIND All Green: `enable-auto-merge` job with `needs: all-green`, preserving the PR #1390 same-repo/fork-safety guard verbatim + the draft skip. The old at-open arming in `auto-merge.yml` is retired (the file survives as a manual-dispatch fallback with the same fork guard re-verified via API) | `ci.yml` `enable-auto-merge`, `auto-merge.yml` |
| 4 | Push-to-main CI runs are NEVER cancelled (grouped by SHA); PR runs keep cancel-in-progress | `ci.yml` `concurrency` |
| 5 | Mutation PR trigger REMOVED — scheduled weekly full sweep + push-scoped runs remain; mutation stays a hard-fail gate on the SCHEDULED lane | `mutation.yml` |
| 6 | `Repo Guards` job runs the self-contained source-scan hooks server-side (banned-pattern, data-integrity, O(1)/dedup, boot-symmetry) on every PR + push — closing the `--no-verify` bypass class. Ratchet-baseline guards (test-count, pub-fn-test, financial) stay local-only: their baselines are gitignored, so a CI run would establish a fresh baseline and pass vacuously (false-OK, audit Rule 11) | `ci.yml` `repo-guards` |
| 7 | Weekly full-tree secret scan (every tracked scan-relevant file, not just diffs) | `secret-scan.yml` `secret-scan-full-tree` |
| 8 | Groww QuestDB E2E gains a path-filtered PR lane (same paths as its push lane) | `groww-e2e.yml` |
| 9 | Branch protection on `main` (operator one-time click): required checks = Build & Verify, Security & Audit, Commit Lint, Secret Scan, **All Green**; require branches up to date; do not allow bypassing (enforce for admins) | GitHub Settings → Branches (no API tool available in-session) |

---

## §4. The honest envelope (mandatory per operator-charter §F — no illusion)

- **Pre-merge gate = the fast battery** (build, clippy, the full 6-crate test suites incl. DHAT + proptest, security audit, coverage ratchet, source-scan guards, plan gate, secret diff-scan). Warm-cache PR wall-clock moves from ~4 min to ~8–10 min (coverage is the new critical path; ~20–25 min on a cold cache).
- **Mutation / fuzz / ASan / TSan / cargo-careful stay SCHEDULED, not pre-merge — by physics, not laziness.** The full mutation sweep over core/trading/common is an ~18-hour-class job (hundreds of mutants, each a build+test cycle); fuzz is 1h/target by design; sanitizers rebuild the workspace with `-Z build-std` single-threaded (hours). None of these can complete inside a per-PR runner budget. They remain hard-fail on their weekly/scheduled lanes with auto-filed issues. Claiming they gate PRs (as mutation.yml's old header did) was a false-OK and is exactly what this lock forbids.
- **`#[ignore]`d QuestDB/Docker tests still skip green inside the PR Test jobs** — the Groww E2E PR lane covers its slice non-vacuously; the chaos suite remains scheduled (and is separately known-broken; not fixed by this lock).
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
- Removes the weekly full-tree secret scan or the Groww E2E PR lane.
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
