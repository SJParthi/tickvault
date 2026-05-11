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

[2026-05-11 20:50 IST] [meta] Task Board pattern adopted to support 5+ parallel Claude Code sessions on Friday. Per-task branch (`claude/task-T<NN>-*`), atomic claim via commit+push race, per-task PR, planning branch stays for planning artifacts only. tasks/_board.md + tasks/T00-example-template.md created. Stale-claim recovery rule documented: 30 min CLAIMED with no commits → STALE, 60 min total → AVAILABLE.

[2026-05-11 20:55 IST] [step-1] Honest envelope for "zero tick loss + WS never disconnects" RE-AFFIRMED per wave-4-shared-preamble.md §8: literal "never disconnect" is IMPOSSIBLE (SEBI 24h JWT + Dhan static IP both force ≥1 reconnect/day). Bounded zero-loss inside chaos envelope IS guaranteed. 22-scenario disconnect matrix enumerated; 8 gaps identified.

[2026-05-11 20:58 IST] [step-1] Operator confirmed full-mode subscription (NIFTY/BANKNIFTY/SENSEX full chain + 216 NSE_EQ ≈ 11,034 instruments under indices_only_all_expiries Wave 5 scope) — this is NOT a documented disconnect cause; it can AMPLIFY backpressure-driven disconnects only. Mac M4 Pro 48GB outspecs AWS c8g.xlarge; bandwidth is the only variable improved by AWS.

[2026-05-11 21:00 IST] [step-1] PHASE 0.5 ADOPTED INTO MEGA PLAN: WS Disconnect Resilience Hardening, 8+1 items closing 8 of 22 disconnect-cause gaps. 5 reserved ErrorCodes promoted (NET-01, NET-02, PROC-01, DH-911, RESOURCE-03) + 3 new (WS-BACKPRESSURE-01/02, TCP keepalive config) + 1 verification of Wave 5 core_pinning wiring. Concrete task files (T05-NN-*.md) deferred to Wed.

[2026-05-11 21:02 IST] [step-1] Mac dev env documented in mega plan: MacBook Pro M4 Pro 14 cores (10P+4E) 48GB. Out-specs AWS c8g.xlarge (4vCPU/8GB) for CPU+RAM. Network only variable improved by AWS Mumbai EIP. Common-runtime principle (same docker-compose Mac=AWS) preserved.

[2026-05-11 21:03 IST] [step-1] CORE-PINNING DESIGN LOCKED: ONE dedicated core for ALL 5 WS connections (async-multiplex), NOT one core per connection. Same 4-core scheme Mac=AWS: Core 0 WS recv / Core 1 parser / Core 2 ILP writer / Core 3 other. Mac wastes 10 idle cores (no harm). AWS c8g.xlarge uses all 4. Item 0.5.9 verifies pinning is wired in main.rs (helper exists per Wave 5 Item 6; wiring unverified).

---

## Reversal procedure

If a decision needs to change:

```
[YYYY-MM-DD HH:MM IST] SUPERSEDED entry #N: <new decision> — reason: <why>
```

Original entry is never edited. Audit trail stays intact.
