# Merge Gate Lock — Nothing Merges Without All Green (Operator Lock 2026-07-04) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/merge-gate-lock-2026-07-04.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before touching `.github/workflows/*` (ci.yml `all-green` / `enable-auto-merge`, auto-merge.yml, mutation.yml, secret-scan.yml), branch protection, or any merge/auto-merge path.** It holds the incident record, the 9-row mechanical contract (§3), the §3.1 GITHUB_TOKEN post-merge catch-up contract, the §3.2 auto-merge-persists-across-updates fix, the §5.1 Rust-only All Green verdict amendment, and the honest envelope. Where this summary and the full file differ, the full file wins. Amend the full file first, then keep this summary in sync.

**Authority:** CLAUDE.md > `operator-charter-forever.md` §H > `pr-completion-protocol.md` > this file > defaults. **Scope:** PERMANENT.

**Operator quote (2026-07-04):** "without going fully green how the fuck most of the PRs got merged... if it is merged means it should have the confidence of it went through a deep drill down in depth thorough check"

**The rule (§2, verbatim):** **A PR merges to `main` ONLY when the `All Green` fan-in job in ci.yml — which needs EVERY PR-relevant job (Build & Verify, all 6 Test crates, Security & Audit, Commit Lint, Design-First Wall, Deploy Lint, Coverage & Perf, Repo Guards) — has succeeded, and auto-merge is armed ONLY after that success, never at PR open.**

**REJECT (§5, verbatim headlines):**
- Removes, renames, or weakens the `all-green` job, or removes ANY job from its `needs:` list without a dated operator quote.
- Adds a new PR-relevant job to ci.yml WITHOUT adding it to `all-green`'s `needs:` list.
- Re-introduces at-PR-open auto-merge arming, or arms auto-merge from any job that does not `needs: all-green`.
- Weakens the PR #1390 same-repo/fork-safety guard on any arming path (`head.repo.full_name == github.repository` is non-negotiable).
- Restores `if: github.event_name == 'push'` (or any PR-excluding condition) on `coverage-and-perf`.
- Restores `cancel-in-progress: true` for push-to-main runs of ci.yml.
- Re-adds the mutation `pull_request` trigger as-is.
- Removes the `Repo Guards` job or any of its four source-scan steps, or converts a guard to `continue-on-error`.
- Removes the weekly full-tree secret scan.
- Treats `skipped` as success in the all-green evaluation for anything other than the PR-only jobs on push events.
- (§5.1, 2026-07-18) The All Green verdict is a **jq+shell** program with BYTE-EQUIVALENT semantics to the retired evaluator, pinned by `scripts/all-green-equivalence-matrix.sh` inside Repo Guards. "The §2 one-line rule, the `all-green` `needs:` list, and every §5 REJECT row are UNCHANGED and **bind the NEW jq evaluator verbatim**" — changing the SETS or the skip semantics inside the jq program is a §5 REJECT without its own dated operator quote.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land."
