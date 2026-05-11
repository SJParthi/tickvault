# Friday 2026-05-15 Mega Plan — Working Directory

> **What this directory is:** The complete planning workspace for
> the Friday 2026-05-15 Claude Code session that will execute the
> 3-phase tickvault sprint (Wave 6 close-out → Wave 7-A4 wire-up →
> live trading observability + OMS go-live).
>
> **Drafted:** Mon 2026-05-11 onwards, via FAANG-style design-review
> discussions between operator (Parthiban) and Claude Opus 4.7 over
> Mon → Tue → Wed evenings.
>
> **Authority chain:** CLAUDE.md > `.claude/rules/project/*.md` > this
> directory's files > defaults.

## Why a directory instead of one file?

| Concern | One mega file | This directory (chosen) |
|---|---|---|
| Crash recovery cost | load 6000 lines | load INDEX.md (~50 lines) |
| Token budget per session | high | low (per-step files ~200 lines) |
| Decision history preserved | no (overwrites) | yes (append-only log) |
| Parallel sessions safe? | no | yes (`.session-lock`) |

## File map (read in this order if cold)

| # | File | Purpose | Read cost |
|---|---|---|---|
| 0 | `README.md` (this) | what + why | once |
| 1 | `INDEX.md` | **WHERE WE ARE NOW** — status, current step, last update, file list | every session start |
| 2 | `00-decisions-log.md` | append-only audit log of every decision (1 line each) | tail only |
| 3 | `01-step-0-why.md` ... `NN-step-N-*.md` | per-step discussion notes (Status, Date, Decision) | only the relevant step |
| 4 | `99-mega-plan-strawman.md` | original Mon strawman, preserved as baseline | reference only |
| 5 | `99-mega-plan-final.md` | rewritten Wed night from all step decisions; **the file the Friday session executes** | once on Friday |
| 6 | `.last-cursor.md` | in-flight state checkpoint (written before every reply) | session resume only |
| 7 | `.session-lock` | optional, if multiple sessions overlap | edit-time |
| 8 | `scripts/recover.sh` | Layer 3 transcript-recovery shell script | only on full crash |

## The 3-layer recovery protocol

```
   New Claude session opens. Operator types "continue planning".
   
   ┌────────────────────────────────────────────────┐
   │ LAYER 1: cheap (~0.01% budget)                 │
   │ 1. git pull origin <branch>                    │
   │ 2. cat INDEX.md                                │
   │ 3. cat .last-cursor.md if present              │
   │ 4. tail -10 00-decisions-log.md                │
   │ 5. cat the step file referenced as "current"   │
   │ → know exactly where we were                   │
   └────────────────────────────────────────────────┘
                       ↓ if cursor stale or missing
   ┌────────────────────────────────────────────────┐
   │ LAYER 2: ~0.05% budget                         │
   │ Read full decisions-log.md + INDEX file list.  │
   │ Compare files-on-disk vs INDEX list (drift     │
   │ detection per scenario 14).                    │
   └────────────────────────────────────────────────┘
                       ↓ if even decisions-log corrupted
   ┌────────────────────────────────────────────────┐
   │ LAYER 3: ~0.1% budget                          │
   │ bash scripts/recover.sh                        │
   │ Reads ~/.claude/projects/-home-user-tickvault/ │
   │ Tails the most recent .jsonl session file.     │
   │ Outputs last 50 user/assistant turns.          │
   │ Reconstruct decisions manually.                │
   └────────────────────────────────────────────────┘
```

## The "Write Before Reply" protocol (MANDATORY for Claude)

Every Claude reply to the operator in this planning workspace MUST:

1. **Atomic-write** `.last-cursor.md` BEFORE sending the chat reply.
   The cursor file contains: topic, step number, last user message
   summary, status (DISCUSSING / DECISION_RECEIVED / DRAFTING_REPLY /
   ABOUT_TO_COMMIT), pending action, timestamp.

2. **If a decision was reached in the user's message**, append to
   `00-decisions-log.md` BEFORE sending the chat reply. Format:
   `[YYYY-MM-DD HH:MM IST] <decision>`.

3. **Commit + push** triggers (any one of):
   - decision reached → commit immediately
   - end of a step's discussion → commit
   - every 5 replies → commit
   - stop-hook fires → commit

This protocol means: if Claude crashes BETWEEN receiving a decision
and sending the next reply, the decision is already on disk.
Recovery cost = 1 sentence repeat at worst.

## Recovery runbooks (for common failures)

| Scenario | Symptom | Fix |
|---|---|---|
| Permission denied writing to dir | `Write` tool errors | `chmod -R u+w .claude/plans/friday-may-15-mega/` |
| Local ahead of remote (un-pushed commits) | `git status` says "ahead by N" | `git push origin <current-branch>` |
| Lost work via force-push to remote | `git log` missing commits | `git reflog` → `git checkout -b recovery-<ts> <sha>` |
| Directory accidentally deleted | files missing | `git checkout HEAD -- .claude/plans/friday-may-15-mega/` |
| INDEX.md drift from real files | INDEX lists files that don't exist | first session action: `ls` + diff; rewrite INDEX |
| `.session-lock` stuck after a crash | new session refuses to edit | check timestamp; if > 30 min old, delete + claim |
| `.last-cursor.md` missing | first action of session | not an error — means clean start |
| Chat compaction loses recent decisions | new session has no memory of last 10 turns | rely on Layer 1 — disk is the source of truth |

## Status conventions (used in every step file)

- **DISCUSSING** — topic raised, no decision yet
- **DECISION_PENDING** — operator considering options
- **DECISION_RECEIVED** — operator answered, not yet written to log
- **DECIDED** — written to decisions-log + step file updated
- **SUPERSEDED** — older decision reversed (new line in log, never edit past)
- **DRAFT** / **REVIEW** / **APPROVED** — for the final mega-plan file
