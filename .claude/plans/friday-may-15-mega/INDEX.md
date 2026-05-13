# INDEX — Friday May 15 Mega Plan workspace

**Plan status:** DISCUSSING (FREE-FORM BRAINSTORM mode — calendar dropped 21:50 IST)
**Current step:** No fixed sequence — operator drives topics, Claude responds. Step 1 (WS resilience + envelope) WRAPPED ✅. Topic pools available for: capital + risk / approval mechanism / anything new operator brings up.
**Last update:** 2026-05-11 21:50 IST (calendar dropped, brainstorm mode locked)
**Mode:** Operator spits thoughts → Claude debates / synthesizes / argues / agrees → decisions land in `00-decisions-log.md` → tables + diagrams persist in topic files → commit + push every session.
**Friday execution target:** 2026-05-15 (retained — when we're ready, we go)
**No implementation rule:** STILL ACTIVE. Brainstorm + design only. Code lands Friday.

**CANONICAL REFERENCE FILES (read in order):**
1. **`topic-PHASE-0-LEAN-LOCKED.md`** — **🔒 LOCKED PHASE 0 PLAN** (operator decision 2026-05-13 ~10:30 IST) — supersedes everything below for Friday build scope
2. `.claude/rules/project/operator-charter-forever.md` — **FOREVER CHARTER** (auto-loaded every session, permanent contract)
3. `.claude/plans/friday-may-15-mega/00-HIGH-LEVEL-PLAN.md` — 5-minute readable summary of pre-lean discussions
4. `.claude/plans/friday-may-15-mega/step-1-honest-envelope.md` — Mon eve honest envelope (canonical reference for tickvault-specific envelope)
5. `.claude/plans/friday-may-15-mega/topic-telegram-message-style-rules.md` — Telegram style rules (auto-driver-level only)

**EVERYTHING ELSE in this directory is now Phase 2 reference material.** Don't load it for Friday build. Phase 0 is self-contained in the LOCKED file above.

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
| `step-1-honest-envelope.md` | CANONICAL | Mon 21:35 IST | **READ FIRST** — operator charter + honest envelope + matrix |
| `step-1-discussion-log-mon-eve.md` | WRAPPED | Mon 21:35 IST | Mon eve transcript summary |
| `step-2-tue-eve-agenda.md` | TOPIC POOL (calendar dropped) | Mon 21:35 IST | 8 questions on capital + risk — pull when topic comes up |
| `step-3-wed-eve-agenda.md` | TOPIC POOL (calendar dropped) | Mon 21:35 IST | 7 blocks A-G (adversarial + approval mechanism) — pull when topic comes up |
| `01-step-0-why.md` | DEPRECATED | — | superseded by `step-1-honest-envelope.md` (numbering reorg) |
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
| L8 | Honest envelope re-affirmed: literal "never disconnect" IMPOSSIBLE (SEBI 24h JWT). Bounded zero-loss guaranteed in chaos envelope. 22-scenario matrix → 8 gaps. | Mon 20:55 IST | log entry #14 |
| L9 | Full-mode 11,034 instruments confirmed NOT a disconnect cause; can AMPLIFY backpressure only. | Mon 20:58 IST | log entry #15 |
| L10 | Phase 0.5 ADOPTED into mega plan: 8+1 items closing 8 of 22 gaps. 5 reserved ErrorCodes promoted + 3 new + 1 verification. | Mon 21:00 IST | log entry #16 |
| L11 | Mac dev env documented: M4 Pro 14c/48GB out-specs AWS c8g.xlarge. Common-runtime principle preserved. | Mon 21:02 IST | log entry #17 |
| L12 | Core-pinning design LOCKED: ONE core for ALL 5 WS conns. Same 4-core scheme Mac=AWS. | Mon 21:03 IST | log entry #18 |
| L13 | Web session 400'd at 21:05 IST. CLI continuation on same branch. Zero context lost (all on disk). | Mon 21:05-21:08 | log entries #19-20 |
| L14 | Item 0.5.10 ADDED: consolidated `PreMarketReady` Telegram (operator pick A). 200 LoC, Wed sub-PR planning. | Mon 21:15 IST | log entry #21 |
| L15 | FAANG 3-day map LOCKED: Mon eve = Step 1 ✅ / Tue eve = Step 2 (capital+risk) / Wed eve = Step 3 (adversarial + APPROVAL). NO implementation Mon-Wed. | Mon 21:20 IST | log entry #22 |
| L16 | Operator charter re-confirmed verbatim. Every plan item carries 15-row + 7-row matrix per per-wave-guarantee-matrix.md. Mechanically enforced. | Mon 21:25 IST | log entry #23 |
| L17 | Telegram screenshot Mon 9:04 PM IST confirms LIVE system streaming (97,422 derivatives, Auth OK, Order WS reconnect working). | Mon 21:30 IST | log entry #24 |
| L18 | Persistence pass: 4 new step files + appended log + INDEX update + cursor + commit + push. Future sessions resume from files. | Mon 21:35 IST | log entry #25 |
| L19 | Step 1 wrapped for Mon eve. NO open Step-1 questions. Tue eve resumes with Step 2. | Mon 21:40 IST | log entry #26 |

---

## Next action

**Tue eve session opens cold** → reads INDEX (this file) → reads `step-1-honest-envelope.md` (canonical) → reads `step-2-tue-eve-agenda.md` (pre-loaded 8 questions on capital + risk) → asks operator Q1 (daily loss budget). Decision per Q goes to log. Move through Q1-Q8. End-of-session: write `step-2-discussion-log-tue-eve.md` + update cursor + commit + push.

---

## How to update this INDEX

Whenever:
- A new step file is created → add row + flip "Status" to IN_PROGRESS
- A step decision is reached → flip step Status to DECIDED, update "Current step" + "Last update" header
- A new file is added/removed → update file ledger
- A decision supersedes a prior one → keep the original row, append note
