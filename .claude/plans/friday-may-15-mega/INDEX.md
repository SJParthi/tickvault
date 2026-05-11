# INDEX — Friday May 15 Mega Plan workspace

**Plan status:** DISCUSSING (Mon-Wed FAANG-style design reviews underway)
**Current step:** Step 0 — "WHY are we building this?" (about to start)
**Last update:** 2026-05-11 20:55 IST (Mon evening — tasks/ subdir added for parallel sessions)
**Total steps planned:** Step 0 through Step ~10 (each ~30-60 min discussion)
**Final mega plan goes live:** Wed 2026-05-13 23:59 IST
**Friday execution starts:** 2026-05-15 06:00 IST

---

## Quick "where are we" pointer

If you are a NEW Claude session opening this directory cold:
1. `git pull origin claude/trading-tick-vault-BkvpS`
2. read this file (you are here)
3. `cat .last-cursor.md` if present (tells you what was happening at last session end)
4. `tail -20 00-decisions-log.md` (recent decisions)
5. read the step file referenced under "Current step" above
6. ask operator: "I see we were on <step>, last decision was <X>. Continue?"

If `.last-cursor.md` is missing → fresh start OR pre-first-reply.
If even `00-decisions-log.md` is corrupted → run `bash scripts/recover.sh`.

---

## File ledger (all files in this directory)

| File | Status | Last updated | Purpose |
|---|---|---|---|
| `README.md` | STABLE | Mon 20:30 IST | what + why + recovery runbook |
| `INDEX.md` (this) | LIVING | Mon 20:35 IST | where we are right now |
| `00-decisions-log.md` | APPEND-ONLY | Mon 20:35 IST | every decision, 1 line each |
| `99-mega-plan-strawman.md` | BASELINE | Mon 18:30 IST | original 10-agent synthesis, preserve as-is |
| `99-mega-plan-final.md` | NOT YET CREATED | — | rewritten Wed night |
| `scripts/recover.sh` | STABLE | Mon 20:35 IST | Layer 3 transcript fallback |
| `.last-cursor.md` | LIVING | (written before every Claude reply) | in-flight state checkpoint |
| `.session-lock` | OPTIONAL | (only if multiple sessions) | concurrency guard |
| `01-step-0-why.md` | NOT YET CREATED | — | first discussion topic |
| `tasks/_board.md` | LIVING | Mon 20:50 IST | live task board for 5+ parallel Claude sessions |
| `tasks/T00-example-template.md` | TEMPLATE | Mon 20:50 IST | copy-this skeleton for new task files |
| `tasks/T<NN>-*.md` | NOT YET CREATED | — | one file per executable task (created Tue-Wed) |

## Verify checklist (run on every session start — scenario 14 hardening)

```bash
cd .claude/plans/friday-may-15-mega/
ls *.md | sort       # compare to file ledger above
                     # any missing/extra files → drift; rewrite this section
```

---

## Steps planned (tentative — each gets its own file when discussion starts)

Each step = 1 small FAANG meeting (30-60 min) → 1 file with kid-friendly explanation + table + decision → 1 line in decisions-log.

| # | Step file | Topic | Status |
|---|---|---|---|
| 01 | `01-step-0-why.md` | WHY does tickvault exist? (mission) | NOT STARTED |
| 02 | `02-step-1-success.md` | success criteria for the project | NOT STARTED |
| 03 | `03-step-2-capital.md` | capital + risk envelope (max loss/day, position size) | NOT STARTED |
| 04 | `04-step-3-instruments.md` | which instruments on Day 1 (NIFTY options? other?) | NOT STARTED |
| 05 | `05-step-4-when.md` | when do we trade (market hours, AMO, pre-open) | NOT STARTED |
| 06 | `06-step-5-strategy.md` | which strategy goes live first | NOT STARTED |
| 07 | `07-step-6-safety.md` | safety harness (kill switch, halt, 15:30 cutoff, P&L exit) | NOT STARTED |
| 08 | `08-step-7-observability.md` | what we monitor + alert + Telegram | NOT STARTED |
| 09 | `09-step-8-deployment.md` | AWS deployment + Mon May 25 cutover | NOT STARTED |
| 10 | `10-step-9-rollback.md` | rollback + disaster recovery for live trading | NOT STARTED |
| 11 | `11-step-10-friday-plan.md` | translate decisions into Friday session todo list | NOT STARTED |

Numbering is suggestive — operator may reorder / add / remove steps.

---

## Locked decisions reference

(Full list lives in `00-decisions-log.md`. Snapshot of what's locked as of last INDEX update:)

| # | Decision | When | Where logged |
|---|---|---|---|
| L1 | 3-phase sequencing: W6 close-out → W7-A4 → live trading | Mon 17:25 IST | log entry #1 |
| L2 | Multi-file structure (this directory) | Mon 20:00 IST | log entry #6 |
| L3 | Files-only persistence, chat = pointers | Mon 17:25 IST | log entry #2 |
| L4 | First live order DEFERRED to Wave 8 (this week: shadow-max) | Mon 19:00 IST | log entry #4 |
| L5 | Write-Before-Reply protocol | Mon 20:25 IST | log entry #7 |
| L6 | Hardened recovery: README + INDEX + decisions-log + cursor + Layer 3 script | Mon 20:35 IST | log entry #8 |
| L7 | Task Board pattern adopted — `tasks/` subdir + `_board.md` + per-task branches + atomic-claim via commit-push for 5+ parallel sessions | Mon 20:50 IST | log entry #13 |

---

## Next action

Operator and Claude start **Step 0 (`01-step-0-why.md`)** discussion in the next chat turn. Claude creates the file with the WHY options + kid-friendly explanation. Operator picks. Decision goes to log. Move to Step 1.

---

## How to update this INDEX

Whenever:
- A new step file is created → add row + flip "Status" to IN_PROGRESS
- A step decision is reached → flip step Status to DECIDED, update "Current step" + "Last update" header
- A new file is added/removed → update file ledger
- A decision supersedes a prior one → keep the original row, append note
