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

[2026-05-11 21:05 IST] [meta] WEB SESSION 400'D — "API Error: 400 messages.150.content.2.text: cache_control cannot be set for empty text blocks" then repeated 400s on retry. Session unusable. Continuing in CLI on same branch `claude/trading-tick-vault-BkvpS`. All decisions persisted to disk pre-400, no context lost. Commit 4f97281 already on origin.

[2026-05-11 21:08 IST] [meta] CLI CONTINUATION begun. Branch switched from `claude/document-disconnect-causes-5r7fo` (CLI default) to `claude/trading-tick-vault-BkvpS` (planning branch). Operator pick A from web (item 0.5.10 PreMarketReady Telegram) carried forward. Commit bf62c99 (cursor update) pushed.

[2026-05-11 21:15 IST] [step-1] ITEM 0.5.10 ADDED (operator pick A in web before 400): consolidated `PreMarketReady` Telegram event replacing the 6 fragmented "Auth OK / Universe loaded / WS 1-5 connected / Phase 2 complete" messages. ~200 LoC, new task file `tasks/T05-10-pre-market-ready-telegram.md` to be created Wed during sub-PR planning. 3 severity tiers: PreMarketReady (Info), PreMarketDegraded (High, ≥1 step failed by 08:55 IST), PreMarketCritical (Critical, NONE green by 09:00 IST).

[2026-05-11 21:20 IST] [meta] FAANG 3-DAY MAP LOCKED: Mon eve = Step 1 (WS disconnect resilience + honest envelope) WRAPPED ✅ / Tue eve = Step 2 (capital + risk engine) / Wed eve = Step 3 (go-live gate + adversarial 3-agent review + mega plan APPROVAL flip). Thu = synthesize. Friday = 5+ parallel Claude sessions execute. NO IMPLEMENTATION Mon-Wed; pick C ("start coding T05-01 now") explicitly rejected by operator.

[2026-05-11 21:25 IST] [step-1] OPERATOR CHARTER RE-CONFIRMED VERBATIM: "extreme complete comprehensive extensive automation 100 percentage" + every-session-auto-everything + every-Cowork-task-auto-everything + access logs/queries/dbs/project entirely local-or-AWS + common-runtime dynamic-scalable + 100% in 15 dimensions (code coverage, audit coverage, testing coverage, code checks, code performance, monitoring, logging, alerting, security, security hardening, bugs fixing, scenarios covering, functionalities covering, code review, extreme check). Persisted in `step-1-honest-envelope.md`. Every Phase 0.5 / Step 2 / Step 3 / Friday task MUST carry the 15-row + 7-row matrix per `per-wave-guarantee-matrix.md`. Mechanically enforced by `per-item-guarantee-check.sh` + `make wave-guarantee-check`.

[2026-05-11 21:30 IST] [step-1] TELEGRAM SCREENSHOT (Mon 9:04 PM IST) confirms LIVE system: 97,422 derivatives loaded, 218 underlyings, Auth OK, Order Update WS connected, Order Update WS reconnected after 1 failure (recovery working), NSE bhavcopy cross-check FAILED (questdb_query_failed — Phase 0.5 NET-02/DH-911 will catch upstream causes), clean shutdown at 07:18 PM, boot complete with 5/5 main feeds DEFERRED until 09:00 IST post-market sleep working. System IS real. Phase 0.5 adds 8 more failure-mode detectors. Mon 21:21 IST post-market reset (`reset_daily()` per Wave-2-D Fix 2) due 15:35 IST — already fired today.

[2026-05-11 21:35 IST] [meta] PERSISTENCE PASS triggered by operator: "don't worry about tokens, persist EVERYTHING to plan files / docs / subs, commit + push, then discuss later." 4 new files written: step-1-honest-envelope.md (canonical reference), step-1-discussion-log-mon-eve.md (Mon transcript), step-2-tue-eve-agenda.md (Tue pre-loaded questions), step-3-wed-eve-agenda.md (Wed pre-loaded blocks A-G). INDEX bumped. Cursor updated. Commit + push next.

[2026-05-11 21:40 IST] [meta] STEP 1 WRAPPED for Mon eve. Tue eve resumes with Step 2 (capital + risk engine, 8 pre-loaded questions). Wed eve = Step 3 (adversarial 3-agent review + APPROVAL flip). NO open Step-1 questions remain — operator's verbatim charter answered, Phase 0.5 locked, 5-layer defence diagram persisted, per-item matrix mandated. Any future session opening cold: read step-1-honest-envelope.md FIRST.

[2026-05-11 21:50 IST] [meta] SUPERSEDED entries 21:20 + 21:40 IST FAANG-3-day calendar: operator dropped the rigid Mon/Tue/Wed/Thu schedule. New mode = FREE-FORM BRAINSTORM. Operator spits thoughts/ideas/decisions/arguments whenever, Claude responds with debate/synthesis. Existing step-2-tue-eve-agenda.md + step-3-wed-eve-agenda.md become TOPIC POOLS (not calendar items) — pull from them anytime. Rules that still hold: (a) NO implementation, (b) every decision persists to files, (c) commit + push every session-end, (d) honest-envelope charter in step-1-honest-envelope.md is canonical, (e) per-item 15-row + 7-row matrix on every plan item, (f) every-session auto-everything via MCP. Friday execution-day target retained — when we get there, we get there.

[2026-05-11 22:00 IST] [topic] JWT + Instruments full audit complete. Written to topic-jwt-instruments-coverage.md. Verdict: JWT 95% covered (7 gaps), Instruments 92% covered (3 design-choice gaps). Awaiting operator pick A/B/C/D on which gaps to close in Phase 0.5.

[2026-05-11 22:10 IST] [topic] FULL-SYSTEM matrix written to topic-full-system-coverage-matrix.md. 3 systems (JWT + Instruments + WS Feed) × 4 time windows × 8 crash modes × 2 paths = 288 sub-scenarios audited. 18 unique gaps identified: 8 closed by Phase 0.5 already-approved items, 2 candidates for Phase 0.5 additions (Items 0.5.11 renewal-task-supervisor + 0.5.12 pool-supervisor-supervisor), 3 to Wave 6 backlog, 5 by-design / out-of-scope. Worst-case envelope confirmed: ≤60s QDB outage absorbed, ≤30s WS gap absorbed, ≤5M-tick rescue ring, DLQ NDJSON for excess. Awaiting operator pick on 0.5.11 / 0.5.12 / persistent rescue ring investigation.

[2026-05-11 22:30 IST] [topic] BYZANTINE / ZOMBIE / "LOOKS HEALTHY BUT ISN'T" matrix written to topic-byzantine-and-zombie-scenarios.md. 68 Byzantine scenarios enumerated across 10 categories: Process-level / Network "TCP open no flow" / Storage "write OK no commit" / Time-clock / Resource exhaustion / False-OK telemetry / Compound cascade / Logical off-by-one / Operator-introduced / Dhan-side silent semantic changes. 24 NEW gaps identified (G19-G46) on top of the 18 from prior matrix. Severity breakdown: 1 Critical (G38 dual-instance live-lock — must close before any live order) + 5 High + 12 Medium + 10 Low. 6 Phase 0.5 additions proposed (0.5.13-0.5.18) to hit 99% Byzantine coverage at ~600 LoC. Honest envelope language preserved. Awaiting operator pick.

[2026-05-11 22:45 IST] [topic] PRE-MARKET READINESS FLOW written to topic-pre-market-readiness-flow.md. 45-min golden window 08:30 → 09:15:30 IST mapped step-by-step. 4-ping Telegram schedule locked: PreMarketReady at 08:33 (18-checkpoint consolidated message replacing 6 fragments) + MarketOpening at 09:00 (WS handshake survived) + Phase2Complete+DepthAnchor at 09:13:30 + MarketOpenStreamingConfirmation at 09:15:30. Degraded + Critical variants designed for failure paths. Mac local = AWS prod identical experience (only "Environment:" field differs). Boot budget: 3 min target / 5 min warn / 25 min HALT at 09:00.

[2026-05-11 22:50 IST] [step-1] ALL 6 PHASE 0.5 ADDITIONS ADOPTED (operator: "add everything into the plans docs"). Phase 0.5 grows from 10 items to 18 items. New items 0.5.11 (renewal-task supervisor, ~80 LoC, Med) + 0.5.12 (pool-supervisor-supervisor, ~60 LoC, Med) + 0.5.13 (🔥 dual-instance live-lock, ~50 LoC, CRITICAL — MUST close before any live order) + 0.5.14 (Tokio deadlock liveness probe, ~100 LoC, High) + 0.5.15 (TCP-flow watchdog per-conn, ~80 LoC, High) + 0.5.16 (Subscribe-ACK parser, ~120 LoC, High) + 0.5.17 (Cross-validation 2-source agreement, ~150 LoC, High) + 0.5.18 (Mid-rebalance replay from subscription_audit, ~100 LoC, High). Total Phase 0.5: ~1,690 LoC. Doable Friday parallel sessions (3-5 sessions × 350 LoC each).

[2026-05-11 22:50 IST] [step-1] Item 0.5.10 LoC estimate raised from 200 → 250 to accommodate MarketOpening 09:00 IST ping + 18-checkpoint state collection + degraded/critical variants.

[2026-05-11 22:50 IST] [meta] COMMON-RUNTIME PRINCIPLE re-affirmed for pre-market flow: Mac dev and AWS prod produce IDENTICAL 4-ping Telegram sequence. Only the "Environment:" line differs in PreMarketReady message ("AWS c8g.xlarge ap-south-1" vs "Mac M4 Pro local"). Same docker-compose, same boot sequence, same checkpoints, same confidence.

[2026-05-11 23:00 IST] [topic] TELEGRAM MESSAGE STYLE RULES locked. Written to topic-telegram-message-style-rules.md. 10 commandments + banned-word list (rkyv/DEDUP/papaya/arc-swap/mpsc/GCRA all banned in operator-facing text) + 4 pre-market pings rewritten in plain English + degraded/critical variants + mid-market alert examples + auto-driver litmus test. New Phase 0.5 Item 0.5.19 added: Telegram Jargon Guard (mechanical scanner, ~80 LoC, Low severity). Phase 0.5 grows to 19 items, ~1,770 LoC.

[2026-05-11 23:10 IST] [meta] FOREVER CHARTER locked at .claude/rules/project/operator-charter-forever.md. Auto-loaded every session via standard rules-directory mechanism. Contains: verbatim operator charter (preserved exactly) + 15-row "100% everything" matrix + 7-row "Resilience" matrix + 10 Telegram commandments + honest 100% claim wording + 11 always-on rules + read-this-first protocol + mechanical enforcement chain. PR template (.github/pull_request_template.md) updated to include 15+7 matrix checkboxes + Telegram style verification + honest-envelope-qualifier check. CLAUDE.md updated to reference the forever-charter as canonical authority.

[2026-05-11 23:10 IST] [meta] BINDING SCOPE: forever-charter applies to (a) every Claude Code session opening this repo, (b) every Claude Cowork task, (c) every new branch claude/*, (d) every PR (template now mandates checklist), (e) every plan item (per-item-guarantee-check.sh enforces matrix), (f) every Friday task on the Task Board. PERMANENT. Cannot be superseded.

[2026-05-11 23:25 IST] [meta] HIGH-LEVEL PLAN written to 00-HIGH-LEVEL-PLAN.md. Single 5-minute readable summary covering: in-one-paragraph mission, 24-hour clock (08:30/09:00/09:13/09:15:30/15:30/16:00 IST), 5 daily Telegrams, 7-day weekly cycle, 365-day yearly handling (holidays/expiries/half-days), 5-layer safety net, 99.9% scenario coverage matrix (90+ scenarios), 19 Phase 0.5 items, Mac vs AWS identical experience, 6 honest confidence guarantees (with envelope), 3-minute boot story, Insta-reel auto-driver summary. INDEX bumped to mark 00-HIGH-LEVEL-PLAN.md as canonical-read-2 alongside operator-charter-forever.md.

---

## Reversal procedure

If a decision needs to change:

```
[YYYY-MM-DD HH:MM IST] SUPERSEDED entry #N: <new decision> — reason: <why>
```

Original entry is never edited. Audit trail stays intact.
