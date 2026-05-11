# Decisions log — APPEND-ONLY

> Every locked decision in the Friday Mega Plan workspace.
> One line per decision. Never edit past entries — to reverse, add
> a new line prefixed `SUPERSEDED:` referencing the original.
> Format: `[YYYY-MM-DD HH:MM IST] [step|meta] <decision text>`

---

[2026-05-11 17:20 IST] [meta] Top priority for week = all three phases in sequence (W6 close-out → W7-A4 wire-up → live trading go-live). Source: AskUserQuestion #1.

[2026-05-11 17:20 IST] [meta] Plan shape = ONE massive single file initially. Source: AskUserQuestion #1. SUPERSEDED 20:00 IST in favour of multi-file directory structure.

[2026-05-11 17:20 IST] [meta] Research output = files only, chat = pointers per stream-resilience.md B1. Source: AskUserQuestion #1.

[2026-05-11 18:30 IST] [meta] Strawman mega plan written to .claude/plans/active-plan-friday-may-15-mega.md (Status: DRAFT) — synthesized from 10 parallel specialist research agents. Commit ed90627.

[2026-05-11 19:00 IST] [meta] First actual live order placement DEFERRED to Wave 8 operator decision. This week's scope max = shadow-equivalence (paper-vs-live diff with dry_run=true still set). Source: AskUserQuestion #2 follow-up.

[2026-05-11 19:00 IST] [meta] Supporting artifacts (ErrorCode stubs etc.) DEFERRED — will be created Fri morning as part of execution, not pre-staged this week.

[2026-05-11 19:30 IST] [meta] Workflow mode = FAANG-style design review meetings Mon/Tue/Wed evening, NOT one-shot plan approval. Operator brings thoughts, Claude pushes back/documents/explains kid-friendly. Approval flip on Wed.

[2026-05-11 19:40 IST] [meta] Latest origin/main pulled into working branch. 2 commits gained: PR #600 (W7-A4.5 DHAT BarCache lookup — COMPLETES Wave 7-A4) and PR #602 (W7-A4.6 bar_cache_loader async HTTP wrapper). Merge commit 92672b6.

[2026-05-11 20:00 IST] [meta] SUPERSEDED 17:20 plan-shape decision: chose Hybrid Option D = multi-file directory with INDEX + decisions-log + per-step files + final mega plan rewritten Wed night. Reason: crash resilience + token budget per session.

[2026-05-11 20:25 IST] [meta] Write-Before-Reply protocol adopted: every Claude reply MUST first atomic-write .last-cursor.md and (if decision reached) append to this log, THEN send chat reply. Commit + push batched on stop-hook / every 5 replies / on decision.

[2026-05-11 20:35 IST] [meta] Hardened recovery protocol approved: 3 layers (cursor → log+INDEX → scripts/recover.sh tailing Claude Code native transcripts under ~/.claude/projects/-home-user-tickvault/). Directory + scripts created.

[2026-05-11 20:35 IST] [meta] Strawman moved into directory as .claude/plans/friday-may-15-mega/99-mega-plan-strawman.md to preserve baseline; original path becomes dead.

---

## Reversal procedure

If a decision needs to change:

```
[YYYY-MM-DD HH:MM IST] SUPERSEDED entry #N: <new decision> — reason: <why>
```

Original entry is never edited. Audit trail stays intact.
