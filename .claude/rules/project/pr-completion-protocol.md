# PR Completion Protocol — Serial Completion (operator lock 2026-05-15)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §H > this file > defaults.
> **Scope:** PERMANENT. Every PR Claude opens in this repo. FOREVER.
> **Operator-locked:** 2026-05-15 (verbatim quote in §H of charter).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## The verbatim operator demand (preserve exactly)

> "always ensure to make it ready for merge once branch pr is created and finished and even always monitor the current PR until it gets merged successfully to main after doing this alone only go ahead with the remaining process always dude okay? always save this bro okay?"

---

## The rule (one line)

**Finish + merge the current PR to `main` BEFORE starting the next item. One PR at a time. Monitor every PR until it lands.**

---

## The 10-step loop (MANDATORY, every PR)

| # | Step | Tool / Command |
|---|---|---|
| 1 | Code complete + local verify | `cargo test -p <crate>` + `bash .claude/hooks/banned-pattern-scanner.sh` + `bash .claude/hooks/pub-fn-test-guard.sh` + `bash .claude/hooks/plan-verify.sh` |
| 2 | Push branch with retry | `git push -u origin <branch>` (4 retries with exponential backoff on network err) |
| 3 | Open PR as **DRAFT** | `mcp__github__create_pull_request` — body MUST include: 9-box checklist, 15-row + 7-row matrices, honest 100% claim with envelope qualifier |
| 4 | Tick plan checkboxes INSIDE this PR's diff | `Edit` on `.claude/plans/active-plan*.md` — `[ ]` → `[x]` for every item this PR completes; commit as part of the PR |
| 5 | Mark PR **ready for review** once CI starts green | `mcp__github__update_pull_request` with `draft=false` |
| 6 | Enable **auto-merge** (squash strategy) | `mcp__github__enable_pr_auto_merge` |
| 7 | **Subscribe** to PR activity | `mcp__github__subscribe_pr_activity` |
| 8 | Event-driven loop: CI red → fix + push; reviewer comment → respond inline or fix | `<github-webhook-activity>` events arrive automatically; do NOT `sleep` poll |
| 9 | Verify merge: `state=closed, merged=true` | `mcp__github__pull_request_read` |
| 10 | Unsubscribe; THEN start next item | `mcp__github__unsubscribe_pr_activity` |

---

## What this rule FORBIDS

| Banned pattern | Why it's banned |
|---|---|
| Opening PR #2 before PR #1 merges | Violates serial completion |
| Marking plan checkbox `[x]` before PR merges to main | The merge IS the completion signal; premature ticks hide failed merges |
| Closing the session while a PR is mid-flight without subscribing | Operator loses visibility |
| `sleep` polling for PR status | Use `subscribe_pr_activity` — webhook events wake the session |
| `--no-verify` / `--no-gpg-sign` on push | Operator never authorized this |
| Manual merge skipping CI | Branch protection blocks; auto-merge respects gates |
| Working on `claude/main` or any branch protected by branch protection | Develop on feature branches only |

---

## Mandatory PR body sections

```markdown
## Summary
<one-paragraph what + why>

## Items completed
- [x] Item N — <one-line>
- [x] Item M — <one-line>

## Z+ 15-row "100% everything" matrix
<copy from operator-charter-forever.md §C, fill in this PR's specifics>

## Z+ 7-row "Resilience" matrix
<copy from operator-charter-forever.md §C, fill in this PR's specifics>

## Adversarial 3-agent review
- [x] hot-path-reviewer pass BEFORE impl (link to comment)
- [x] security-reviewer pass BEFORE impl
- [x] general-purpose hostile pass BEFORE impl
- [x] all 3 agents repeat AFTER impl on the diff

## Honest 100% claim
100% inside the tested envelope, with ratcheted regression coverage: <list specific envelope bounds with constants + ratchet test files>.

## Test plan
- [x] `cargo test -p <crate>` green
- [x] banned-pattern scanner clean
- [x] pub-fn-test-guard clean
- [x] plan-verify clean
- [x] CI all green
```

---

## Commit-body quoting rule — section signs (AM-r1 F9, binding 2026-07-10)

Two PUSHED commit bodies (f71e31541 "$2", e45a31881 "$36.3") permanently garbled
`§2` / `§36.3` citations because `git commit -m "..."` in a DOUBLE-quoted shell
string expands `$2` / `$36` as positional parameters. In this repo, dated
section citations in commit bodies ARE the authority chain — the garble is an
unfixable-post-push provenance blemish (history rewrite / force-push banned).

**The rule:** any commit message body containing `§N` (or any `$`-adjacent
text) MUST be written via a SINGLE-quoted `-m '...'`, a heredoc, or
`git commit -F <file>` — NEVER a double-quoted `-m "..."`. After committing,
verify with `git log -1 --format=%B | grep -c '§'` when the body cites
sections. (Corrected citations for the two garbled commits are recorded in
`.claude/plans/archive/2026-07-10-futidx-allmonths.md` AM-R1 F9 — archived from
active 2026-07-13 per plan-enforcement rule 7.)

## When CI fails mid-loop

The session stays subscribed. On each `<github-webhook-activity>` CI-fail event:

1. Read the failing job log (`mcp__github__get_commit` or `pull_request_read`)
2. Diagnose root cause (do NOT blanket-retry — that's `sleep` polling in disguise)
3. Push the fix to the same branch
4. Auto-merge will fire on green; subscription stays alive

If the same fix has been pushed 3 times and still fails, reply on the PR with the diagnosis + ask the operator before pushing again.

---

## When a reviewer comments

1. Read the comment via `mcp__github__pull_request_read`
2. Decide: fix inline (push commit) OR reply with grep evidence (false positive) OR `AskUserQuestion` if ambiguous
3. After fix → resolve the review thread via `mcp__github__resolve_review_thread`
4. Auto-merge re-fires once all "requires action" review threads are resolved + CI green

---

## Mechanical ratchets (to be wired)

| Ratchet | Status |
|---|---|
| `.claude/hooks/serial-pr-guard.sh` — refuses to start new code if open PR exists on the operator's repo | PLANNED |
| `crates/storage/tests/serial_pr_protocol_guard.rs` — greps for this file's canonical phrases | PLANNED |
| Banned-pattern scanner category for `--no-verify` / `--no-gpg-sign` in shell history | PLANNED |

---

## Auto-driver / Insta-reel explanation

> "Sir, imagine your juice shop. Old way: you start making orange juice, then in the middle you start mango, then in the middle of mango you start sweet-lime. Three half-juices on the counter, nothing finished. Customer angry. New way: ONE juice at a time. Make orange → pour → hand to customer → take payment → only THEN start mango. Every juice gets the checkmark on the list ONLY when the customer pays. Three juices = three customers paid = three checkmarks. Simple. No half-juices on the counter."

---

## Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Opens any PR
- Edits `.claude/plans/active-plan*.md`
- Pushes any branch
- Uses any `mcp__github__*` tool
