# Operator Session Defaults — every session, every task (Operator Lock 2026-07-10)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file > defaults.
> **Scope:** PERMANENT. Every Claude Code session, every Cowork task, every phase of every task, every PR.
> **Operator-locked:** 2026-07-10 (verbatim quote below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demand (preserve exactly, do not paraphrase)

**Quote (2026-07-10):**
> "Run EVERY phase of whatever the task is as multi-agent workflow fan-outs, never solo... open a DRAFT PR and drive CI fully green; DO NOT MERGE — the coordinator session merges. Golden Rule #10: evidence every claim, report green/partial/not-done honestly."

---

## §1. The 8 mechanical rules

1. **MULTI-AGENT ALWAYS.** Every phase of every task runs as parallel multi-agent fan-outs, never solo: parallel readers → 3 independent designers + a judge panel → implement → 4+ hostile reviewers in parallel (correctness, security, O-complexity honesty, coverage honesty, edge cases) → 3 adversarial refuters attack every finding, looping until 2 consecutive clean rounds → all local gates BEFORE opening the PR → one final independent workspace-health agent runs the real suites and reports captured evidence.

2. **PRs.** Open as DRAFT, drive CI fully green, then mark ready. Do NOT merge manually and do NOT arm auto-merge yourself — merges happen only via the repo's post-All-Green flow or the coordinator session, and only with `All Green` = success on the EXACT final head SHA (see `merge-gate-lock-2026-07-04.md` §3.2). Monitor the PR until merged; resolve conflicts/CI failures and continue.

3. **EVIDENCE EVERY CLAIM.** Label every claim Verified / Assumed / Risk / Unknown; paste real test output and real numbers; report green / partial / not-done honestly; no false-OK signals (audit-findings Rule 11).

4. **COVERAGE.** Cover extreme permutations, exceptions, worst cases, and out-of-box scenarios. O(1) uniqueness / dedup / mapping / latency / time / space wherever claimed — honestly flag anything non-O(1) with the constraint and the chosen alternative; never fake O(1). (Cross-ref `engineering-execution-standard-2026-06-26.md` §4.)

5. **100% AUTOMATED.** No human input in any delivered mechanism; every invariant is enforced by a build-failing ratchet test or a real alarm, never a promise.

6. **DELIVERABLES.** Plain chat tables readable by anyone — never HTML, never `.md` files as the answer.

7. **NEVER BLOCK.** Never block waiting for operator input; proceed autonomously on everything reversible.

8. **REPORT.** Report progress and the final "ready to merge, head `<sha>`, All Green success" to the coordinator session.

---

## Trigger (auto-loaded)

Always loaded (this file lives under `.claude/rules/project/`). Reinforced on every session start, every task phase, and every PR.
