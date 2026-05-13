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

[2026-05-11 23:40 IST] [topic] STRAWMAN UPDATED with all 19 Phase 0.5 items severity-sorted (1 Critical + 11 High + 4 Medium + 3 Low = 1,770 LoC). Items 0.5.10-0.5.19 (added Mon eve after strawman first written) now precisely listed with severity tags + LoC + ErrorCode + Files-to-touch. Friday parallel-session distribution proposed: 5 sessions × ~350 LoC each.

[2026-05-11 23:42 IST] [topic] TELEGRAM TIMELINE 08:30→09:15:30 written to topic-telegram-timeline-0830-to-0915.md. Operator's "show me exact messages 8:30 till 9:15 AM" answered: 4 ping schedule with FULL message bodies (PreMarketReady 08:33 with 18-checkmark consolidation / MarketOpening 09:00 / Phase2Complete 09:13:30 / MarketOpenStreaming 09:15:30) + degraded + critical variants + before/after comparison with today's 6-fragment Telegram screenshot. Daily Telegram budget on healthy day = 5 messages (4 pre-market + 1 cross-verify at 16:00). Weekend/holiday = 0 messages.

[2026-05-12 00:00 IST] [topic] TELEGRAM RICH VISUALIZATION written to topic-telegram-rich-visualization.md. Operator flagged plain-text Telegrams as boring. Proposed Phase 0.5 Item 0.5.20 — Rich Notification Renderer with 5 sub-items: (a) HTML parse_mode + Unicode progress bars + text sparklines (80 LoC), (b) PNG charts via plotters crate + sendPhoto (200 LoC), (c) animated GIFs via sendAnimation + 5 asset files (100 LoC), (d) inline keyboards + callback handler (150 LoC), (e) live message editing via editMessageText (200 LoC). All 4 pings re-designed in rich format with concrete HTML examples. Total Item 0.5.20 = 730 LoC, all Low severity (style polish). Phase 0.5 now optionally 20 items, ~2,500 LoC. Awaiting operator pick A (full 730 LoC) / B (just text+GIFs 180 LoC) / C (skip, do Wave 6) / D (different variant).

[2026-05-12 00:10 IST] [step-1] PICK A LOCKED: full Phase 0.5 Item 0.5.20 rich treatment (a+b+c+d+e = 730 LoC). Operator: "this animation looks extremely awesome — pick A — this is how everyone would expect like Insta reels or marketing."

[2026-05-12 00:15 IST] [topic] CRISIS / OUTAGE / BREAKAGE VISUALIZATION written to topic-telegram-crisis-visualization.md. 5-tier severity visual hierarchy locked: 🟢 INFO (static GIF, default sound, no pin) / 🔵 NORMAL (subtle pulse) / 🟡 WARNING (pulse + vibrate) / 🔴 CRITICAL (flashing siren + URGENT sound + pinned + auto-repeat every 5 min) / 🚨 EMERGENCY (strobe + SMS + pinned + auto-repeat every 2 min + escalates). 8 concrete crisis scenarios designed (WS-all-dropped, auth-expired, OOM, disk-full, cross-verify-fail, Dhan-outage, zombie-deadlock, risk-breach). Cascade coalescer designed (3 alerts in 60s → 1 consolidated message with root-cause hint). Auto-escalation chain locked (T+5min repeat / T+10min SMS / T+15min email / T+30min voice-call future). Recovery celebration messages designed. Item 0.5.20 expanded with 3 new sub-items (f severity-tier visual, g cascade coalescer, h recovery celebration) bringing total to ~1,160 LoC. Phase 0.5 grows to 20 items, ~2,930 LoC. Friday Session 5 absorbs 510 LoC (a+c+f+h), Wave 6 polish gets 650 LoC (b+d+e+g).

[2026-05-12 00:30 IST] [topic] HONEST 99.9% AUDIT written to topic-coverage-honest-audit.md. PUSHBACK on operator's "is Telegram alone enough for 99.9%" — answer NO. Telegrams = ~10% of coverage (operator-visibility layer). True coverage is 7-layer iceberg: PREVENT (ratchets) → ABSORB (rescue ring) → DETECT (watchdogs) → RECOVER (supervisors) → AUDIT (tables) → ESCALATE (alerts) → AWARENESS (Telegrams). Today after Phase 0.5 = 88%. With rich Telegrams (0.5.20) = 92%. 25 NEW gaps surfaced (G48-G72) across 6 categories: Telegram-delivery-itself-fails (6 gaps), tickvault-process-dies-silently (4 gaps), strategy-logic-bugs (4 gaps), black-swan-market (4 gaps), dependencies/supply-chain (4 gaps), operational/human (3 gaps). Proposed 10 new items 0.5.21-0.5.30 (~1,460 LoC): external health-check, SMS fallback, out-of-process watchdog, strategy-decision audit log, corporate-action detector, runtime CVE scanner, config drift detector, backup operator, multi-channel alerts, external time-sync probe. Coverage curve: 88% → 92% → 97% → 99%. TRUE 99.9% requires enterprise dual-region — overkill for solo retail. HONEST REVISED CLAIM: "97% realistic target with strong audit trail for 3% edge cases". Awaiting operator pick A (all 10 new = 99%) / B (3 critical = 95%) / C (stop at 20 items = 92%).

[2026-05-12 00:50 IST] [step-1] PICK B LOCKED (operator delegated decision). ADD 3 critical items 0.5.21 (external health-check, 50 LoC), 0.5.22 (SMS fallback, 150 LoC), 0.5.23 (out-of-process watchdog, 200 LoC). Total +400 LoC. Coverage target: 95% inside tested envelope. Items 0.5.24-0.5.30 (7 items, ~1,060 LoC) DEFERRED to Wave 6 polish. Honest claim revised: "95% inside the tested envelope + audit trail for the 5% edge cases. True 99.9% out of scope (enterprise dual-region required, overkill for solo retail)." Phase 0.5 FINAL TOTAL: 23 items, ~3,330 LoC, distributable across 6 Friday parallel sessions × ~550 LoC each.

[2026-05-12 morning IST] [topic] TOP-N VOLUME DYNAMIC DEPTH plan written to topic-top-n-volume-dynamic-depth.md. Operator asked how dynamic depth-20 (5×50=250) + depth-200 (5×1=5) subscription works post-09:15. CONFIRMED: code partially exists (`depth_dynamic_top_volume_selector.rs` + `depth_dynamic_pipeline_v2.rs`), config exists in base.toml lines 308-328. 4 operator decisions surfaced: Q1 ranking metric (config says `change_pct_desc`, operator said VOLUME — mismatch), Q2 cold-start strategy (ATM bootstrap recommended for 09:00-09:16 window), Q3 re-rank frequency (60 sec + 1.1× hysteresis recommended), Q4 tie-breaker (ATM-distance recommended). Hidden problem flagged: pure top-5 by volume for depth-200 may give all 5 slots to NIFTY (3-4× BANKNIFTY volume) — operator must decide open cohort vs forced 3+2 split. Proposed new Phase 0.5 item ~200 LoC: verify boot wiring + flip rerank_metric + add hysteresis + add DepthAutoRanked Telegram + ratchet test for capacity. Awaiting operator pick A (accept all 4 recommendations) / B (force depth-200 split) / C (answer Q1-Q7 individually) / D (deep-dive code flow).

[2026-05-12 morning IST] [topic] OPTIONS VOLUME DATA AVAILABILITY clarification written to topic-options-volume-data-availability.md. Operator caught a timing issue. CORRECTED my earlier design: F&O has NO pre-open session (unlike equities). Options trade 09:15-15:30 ONLY. So options volume = 0 from 00:00 until 09:15:00 IST sharp. First option tick = 09:15:00.000. First top-volume ranking possible = 09:16:00 (after first 1-min movers_1m window). Cold-start window for depth-20/200 top-volume swap is REALLY 09:15-09:16 (1 minute), not 16 minutes as earlier suggested. Volume field semantics confirmed: bytes 22-25 of Quote/Full packet = cumulative-day, resets to 0 each 09:15 IST. NO need for yesterday's volume — fresh-today self-contained. Recommended rolling 60-sec window (matches existing `movers_1m`). Recommended `min_liquidity_volume = 0` (top-N sort enforces ordering). Updated Phase 0.5 item bumped to ~235 LoC.

[2026-05-12 morning IST] [topic] DYNAMIC DEPTH FINAL LOCKED DESIGN written to topic-dynamic-depth-locked-design.md. Operator clarified 3 critical points: (1) NO ATM bootstrap — depth-20/200 idle pre-09:15, first swap at 09:16:00 sharp, (2) DELTA-ONLY swaps — only unsub/sub the slots that changed, NOT blanket unsub-all + sub-all, (3) Depth-200 FORCED SPLIT (not open cohort): Slot 1 = top NIFTY CE, Slot 2 = top NIFTY PE, Slot 3 = top BANKNIFTY CE, Slot 4 = top BANKNIFTY PE, Slot 5 = wildcard (next-highest excluding slots 1-4). Both NIFTY + BANKNIFTY guaranteed coverage. 20-scenario worst-case matrix mapped (WC1-WC20) covering empty cohort, dedup conflicts, stale mat view, expiry rollover, NSE circuit halt, etc. 3 NEW ErrorCodes proposed: DEPTH-DYN-04 (empty cohort), DEPTH-DYN-05 (stale mat view), DEPTH-DYN-06 (degenerate single-SID cohort). Friday Phase 0.5 task bumped from 200 → 880 LoC. Phase 0.5 grand total now 24 items / ~4,210 LoC (was 23 items / 3,330 LoC).

---

## Reversal procedure

If a decision needs to change:

```
[YYYY-MM-DD HH:MM IST] SUPERSEDED entry #N: <new decision> — reason: <why>
```

Original entry is never edited. Audit trail stays intact.

## 2026-05-12 04:42 IST — WS Flow Health 7-Layer Defense LOCKED

Operator hit "Connected ≠ Streaming" LIVE — only manual `make stop && make run` fixed it.
Root cause hypothesis #1: Dhan-side conn counter stale for 60–120s after TCP-RST.

Locked design captured in `topic-ws-flow-health-7-layer-defense.md`:

| Decision | Lock |
|---|---|
| Layers | L1 frame-rate + L2 sub-ACK + L3 daily reconcile + L4 token-age + L5 ping audit + L6 soft restart + L7 cooldown |
| Cooldown | Hybrid: 5s first attempt, 30s retry |
| Restart mode | Internal soft restart (Tokio survives, WS layer rebuilt) |
| Reconcile time | 09:14:30 IST daily |
| Scope | All 16 WS conns (5 main + 5 depth-20 + 5 depth-200 + 1 order-update) |
| Total LoC | ~1,790 |
| NEW ErrorCodes | WS-FLOW-01 (dead-air), WS-FLOW-02 (recovered), AUDIT-07 (recovery audit fail) |
| NEW table | `ws_recovery_audit` (DEDUP UPSERT KEYS(ts, conn_id, event)) |
| NEW metrics | 8 Prom metrics (frame-rate gauge + recovery counters + cooldown histogram) |
| NEW alerts | 5 alert rules in `alerts.yml` |
| NEW panels | 5 Grafana panels in `operator-health.json` |
| Rollout | Wave A (L1+L7) → B (L2+L4) → C (L3+L5) → D (L6) — incremental |
| Friday session | Own ~10–12hr session, NOT bundled with dynamic depth |

Pending: operator `LGTM` to flip DRAFT → APPROVED.

## 2026-05-12 10:25 IST — LIVE LOG diagnosis confirms 7-Layer plan correctness

Operator ran app at 10:14 IST. Log analyzed in
`topic-live-log-analysis-2026-05-12-10am.md`.

**Key events observed:**
- 10:14:50 — Clean 40.5s boot
- 10:15:12 onwards — illiquid instrument tick-gap noise (1 → 156 instruments)
- 10:18:10 / 10:21:15 / 10:21:55 — SLO-02 flapping (correctly edge-triggered)
- **10:21:15 — DHAN-SIDE MASS TCP RST**: 9 of 10 WS endpoints RST'd in 1 second
  (5 depth-20 + 4 main feed). Conn #3 NOT in storm — went silent instead.
- 10:21:16 — All 9 reconnected in <200ms (SubscribeRxGuard PR #337 worked)
- 10:21:21 — 910 instruments had 30-281s gaps (post-reconnect blind window)
- 10:22:14 — WS activity watchdog fired on conn #3 (silent 54s)
- 10:19:46 — DEPTH-DYN-V2-01 cohort empty (RAM-empty after boot grace)

**Plan coverage verdict (HONEST):**
| Symptom | Z+ plan covers? |
|---|---|
| Mass TCP RST | ✅ L7 30s cooldown + L6 soft restart |
| Conn #3 silent socket | ✅ L1 frame-rate gauge (5s window) — would catch in 10s instead of 54s |
| 30s blind window after reconnect | ✅ L2 subscribe-ACK round-trip |
| Illiquid tick-gap noise | ❌ Need liquidity filter (Task 13 retrofit) |
| DEPTH-DYN-V2-01 cohort empty | 🟡 Accept cold-start cycle OR extend boot grace |
| Prometheus `hyper::Error(IncompleteMessage)` | ❌ NEW topic needed — observability 7-layer |

**Action items (NO IMPLEMENTATION — plan-only):**
1. Update `topic-ws-11-root-causes-ratchet-spec.md` — add Scenario #12 (Dhan-side mass RST)
2. Update `topic-ws-flow-health-7-layer-defense.md` — L7 cooldown 30s HARD (not hybrid)
3. NEW topic `topic-observability-7-layer-z-plus-defense.md` — Prometheus exporter resilience
4. Email Dhan support — ask about mass-RST trigger interval
5. Update Task 13 retrofit — WS-GAP-06 liquidity filter

Honest envelope claim updated: "Inside the tested envelope, mass-RST recovery
≤ 30s (L7 cooldown + L2 subscribe-ACK), single-conn silent-socket detection
≤ 10s (L1 frame-rate gauge). Beyond envelope, soft restart (L6) within 90s."

## 2026-05-12 10:45 IST — ZERO TICK LOSS COVERAGE MAP locked

Operator demand: "not even a single tick data loss is acceptable"

Locked TWO new files:

**1. `topic-zero-tick-loss-coverage-map.md`** — Master map
- 8 stages: Network → Parser → Channel → Processor → Persist → Retention → Replay → Cross-verify
- 59 loss paths identified, 59 defended ✅
- Each defense has a ratchet test path
- Honest envelope claim mandated

**2. `topic-observability-7-layer-z-plus-defense.md`** — G1 (NEW)
- 11 observability failure paths mapped
- 7-Layer Z+ pattern applied (77-cell matrix)
- 7 NEW metrics + 7 alerts + 7 panels
- NEW audit table: `observability_health_audit`
- Belt+suspenders Telegram (Alertmanager + direct path)

**Honest envelope wording (mandatory):**
> "Zero tick loss inside the tested envelope:
> Stage 1: ≤30s mass-RST recovery; ≤10s silent-socket detect; ≤80s soft restart
> Stage 2: compile-time ratchets
> Stage 3: 5M-tick rescue ring (TICK_BUFFER_CAPACITY)
> Stage 4: O(1) hot path, DHAT zero-alloc, composite-key
> Stage 5: 3-tier rescue→spill→DLQ; ≤60s QuestDB outage
> Stage 6: dry-run + hard floor (refuse drop date >= today-30d)
> Stage 7: per-record checksum + DEDUP idempotency
> Stage 8: daily bhavcopy + Option Chain reconcile
>
> Beyond envelope: NDJSON DLQ catches every payload as recoverable text.
> SEBI 5y S3 retention guarantees long-term recovery.
>
> Mathematically impossible literal: SEBI 24h JWT mandates >=1 disconnect/day.
> 'WS never disconnects' is a physics violation. Our envelope handles it."

Total Friday workload now: ~6,000 LoC + zero-tick-loss coverage map +
observability 7-layer + 11 root-cause ratchets. ~3 days x ~3 sessions.

Pending: operator LGTM to flip all DRAFT → APPROVED status.

## 2026-05-12 10:50 IST — data/ws_wal/ DISCOVERY = Stage 1.5 added to ZTL map

Operator screenshot of IntelliJ project structure revealed `data/ws_wal/`
directory with 4 active `.wal` files. This is a layer I did NOT have in
my mental model.

**Implication:** every raw WS frame is persisted to disk as binary WAL
BEFORE parsing. This is the STRONGEST possible defense against tick loss
— even if all downstream stages fail, raw bytes are on disk.

**Coverage map update:** Stage count 8 → 9 (added Stage 1.5 — WS WAL capture).

**New audit table:** `ws_wal_audit` (NEW — write events, rotation, retention).

**Open questions (Friday verification needed):**
1. Per-conn or per-feed WAL files? (4 files visible, 16 endpoints)
2. Sync vs async writes? Flush interval?
3. Retention policy? Disk math: ~14 GB/trading day
4. Is replay tool already built? grep `replay_ws_wal`
5. Is WAL on hot path? DHAT verify ≤10µs write latency

**Updated honest envelope claim (mandatory):**
> Zero tick loss inside the tested envelope, with belt-and-suspenders:
> - Primary path (Stage 1 → 8): 7-Layer Z+ across 8 stages
> - 🆕 Black-box recorder (Stage 1.5): raw WS frames persisted to
>   data/ws_wal/ BEFORE parsing. Even if all downstream fails, raw bytes
>   are on disk for replay.
> - Catastrophic envelope: ws_wal disk full AND QuestDB down AND rescue
>   ring full AND spill disk full SIMULTANEOUSLY → 5M-tick in-memory ring
>   still buffers.
> - Beyond catastrophic: requires 4 simultaneous disk failures + 2 process
>   failures. Not software-recoverable.

This is a MEANINGFULLY STRONGER guarantee than the previous map.

Locked in `topic-data-directory-architecture-map.md`.

## 2026-05-12 11:00 IST — DEEP DRILL on memory/WAL/ring/shadow tables

Operator screenshots revealed:
1. 37 QuestDB tables incl **9 shadow tables** (parallel persistence)
2. Live Telegram at 10:31 AM fired EXACT pattern we're designing for:
   "[CRITICAL] zero live ticks during market hours, Silent for 257s (threshold 120s)"
3. "[HIGH] Depth spot price STALE BANKNIFTY Age 257s threshold 180s"
4. TWO mass-RST rounds at 10:21 + 10:31 = **9 min interval** (load-balancer rotation hypothesis)

**Locked `topic-memory-wal-ring-shadow-deep-drill.md`:**
- 4 sub-systems × 11 paths each = **44 NEW paths defended**
- 4 sub-systems × Z+ 7-layer = 28 cell defenses
- Existing defenses CONFIRMED:
  * `[CRITICAL] zero live ticks` alert (threshold 120s) ✅ exists
  * Depth spot stale detector (threshold 180s) ✅ exists
  * Shadow tables (9) ✅ exist
  * `aggregator_seal_audit` table ✅ exists

**Combined with prior coverage map: 103 total tick-loss paths defended.**

**3 CRITICAL gaps surfaced from screenshots:**
- GAP A: Threshold 120s → need 30s (with per-instrument liquidity filter)
- GAP B: 9-min mass-RST pattern → need Dhan support email
- GAP C: Depth spot stale 180s → need 30s (same liquidity filter)

**Updated honest envelope (FINAL form — STRENGTHENED):**
> Zero tick loss inside tested envelope, with quintuple coverage:
> 1. Primary 7-Layer Z+ across 9 stages
> 2. Stage 1.5 Black-box WS WAL recorder (raw bytes on disk pre-parse)
> 3. 5M-tick rescue ring (83s peak / 333s real-world headroom)
> 4. 9 shadow tables (parallel persistence)
> 5. NDJSON DLQ (final catch)
>
> Memory bounded: 2 GB app cap + 2 GB host headroom (hard floor)
> Beyond envelope: requires 5 simultaneous failures.
> SEBI 24h JWT: >=1 disconnect/day by law — handled by L1+L2+L7 in <=30s.

Discussion mode continues. 3 days. No code.

## 2026-05-12 11:05 IST — Tick → Candle math + Token/Auth Z+ + Grafana observations

Operator decision LOCKED:
1. **6 timeframes** (was 9): `1m, 3m, 5m, 15m, 1h, 1d`. DROP: 1s, 30m, 2h, 3h, 4h. ADD: 3m.
2. **Direct-from-ticks aggregation** — each TF runs independent in-memory aggregator; no cascade. Retires `candles_1s` base.
3. **prev_day_close** clean:
   - IDX_I: PrevClose packet (code 6) bytes 8-11
   - NSE_EQ + NSE_FNO: `close` field in Quote (38-41) / Full (50-53)
4. **prev_day_oi QUADRUPLE strategy:**
   - P1 NSE bhavcopy CSV (gold — official EOD)
   - P2 Dhan Option Chain REST overlay (silver — already wired, 873 today)
   - P3 yesterday's last-tick OI from `previous_close` table (bronze)
   - P4 first tick of day = prev_day_oi (fallback)

**3 NEW files locked:**

1. `topic-tick-to-candle-math-prevday-sourcing.md` — 6-TF direct-from-ticks math
2. `topic-token-auth-7-layer-z-plus-defense.md` (Option C deferred) — 11 root causes × 7 layers = 77 cells; 4 independent renewal defenses
3. `topic-grafana-dashboard-observations.md` — Market Data Explorer reference

**3 NEW gaps surfaced from Grafana screenshots:**
- Gap A: `build_time = 1970-01-01 05:30:00` (epoch bug, cosmetic)
- Gap B: `index_constituents` table empty (boot WARN "no constituency cache")
- Gap C: Index Summary + Index Constituents panels empty (depends on B)

**Discussion items requiring operator decision (D1-D7 tick math + D1-D6 token + D1-D4 dashboard):**
- 1h alignment 09:15-base vs clock-hour
- Whether to keep 1s for HF strategy
- Daily candle seal time (15:30 vs 15:35)
- Late tick handling (drop vs merge)
- Cross-TF reconcile cadence
- prev_day_volume mirror strategy
- TOTP clock-drift strictness
- IP allowlist primary + secondary
- All-WS subscribe-refresh on token rotate
- Constituency source-of-truth (niftyindices vs nseindia vs Dhan)

**Total Friday brainstorm output so far: 11 plan files. 3 days remaining for discussion.**

NO IMPLEMENTATION. Discussion mode continues.

## 2026-05-12 11:45 IST — POST-MARKET FETCH = THE HEART PIECE

Operator decisions LOCKED (7 Q's answered):

| # | Question | Decision |
|---|---|---|
| Q1 | TFs to fetch | ONLY 1m. Higher TFs aggregated locally from 1m. |
| Q2 | Cross-verify tolerance | 0% zero. Per (ts,tf), OHLCV+OI must match EXACTLY. |
| Q3 | Resume on failure | Resume from last successful instrument. NEVER skip. Retry until success. |
| Q4 | Mock Trading + weekends/holidays | Trading days + Mock Sessions only fetch. Others = no-op. |
| Q5 | Final candle | 15:29:00 → 15:29:59 INCLUSIVE (live aggregation source-of-truth) |
| Q6 | Dhan historical vs NSE bhavcopy | BOTH. Different roles. Dhan @ 15:31 IST same day for cross-verify; bhavcopy @ 08:15 IST next morning for independent prev_day_oi backup. |
| Q7 | Trigger | Post-market only. Wait for all 16 WS conns CLOSED. ONCE per day on successful complete fetch. |

Locked plan: `topic-post-market-historical-fetch-cross-verify.md`:

**3 NEW QuestDB tables:**
- `historical_fetch_state` (per-instrument progress, DEDUP UPSERT KEYS)
- `cross_verify_mismatches` (per-field mismatch audit, SEBI 5y retention)
- `cross_verify_summary` (per-day rollup, SEBI 5y retention)

**8 NEW Prom metrics + 4 NEW alerts + 6 NEW Grafana panels + 4 NEW Telegram events.**

**15 worst-case scenarios W1-W15 all defended.**

**Idempotency proof:**
- Survives process death (state in QuestDB)
- Survives AWS shutdown (resume next boot)
- Survives concurrent runs (DEDUP UPSERT)
- Never duplicates work (success rows skipped)
- Never misses data (resume from last failed)

**Zero-tolerance enforcement:** mismatches table is PER-FIELD. Single SQL filter
shows all OI mismatches, all volume mismatches, etc.

**Dhan/bhavcopy resolution:** BOTH run unconditionally:
- Dhan historical @ 15:31 = cross-verify capability (needs full OHLCV+OI)
- NSE bhavcopy @ 08:15 next morning = independent backup for prev_day_oi
- They cross-check each other on OI

**Operator's worst-case worry (WS fail at 3:29 PM):** Dhan historical fetch
still has 15:29 candle on Dhan-side; cross-verify catches the live gap;
NSE bhavcopy is third backstop.

**6 D-items still need operator pick** (D1-D6 in plan file):
- D6 (fetch 1d historical too as cheap insurance) — my vote YES
- D4 (Mock Trading Saturday Dhan behavior) — need Dhan email
- D3 (AWS shutdown vs wait for fetch) — my vote accept shutdown

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 12:10 IST — bhavcopy PRIMARY for prev_day_* + D-items resolved

### Bhavcopy decision (operator-locked)

**Operator's call:** NSE bhavcopy is PRIMARY for prev_day_oi, prev_day_high, prev_day_low, prev_day_close (where not from ticks).

**Key insight locked:** bhavcopy + Dhan historical serve DIFFERENT roles, not redundant:
- bhavcopy = DAILY granularity, all instruments in 1 download → prev_day_* fields
- Dhan historical = MINUTE granularity, per-instrument → 1m cross-verify

Strategy:
- prev_day_close (IDX_I): PrevClose packet (free from ticks)
- prev_day_close (NSE_EQ/FNO): close field from Quote/Full (free from ticks)
- prev_day_high/low: NSE bhavcopy (no tick equivalent)
- prev_day_oi: NSE bhavcopy (operator's "best case")
- prev_day_volume: NOT NEEDED (operator confirmed earlier)

### All D-items resolved (post-market plan):

| # | Decision |
|---|---|
| D1 | `/v2/charts/intraday` endpoint locked |
| D2 | Write prev_day cache AFTER cross-verify (only authoritative values) |
| D3 | AWS shutdown NEEDED — accept it, resume next boot if fetch incomplete |
| D4 | Mock Trading sessions: Dhan provides data — fetch same as regular days |
| D5 | Timestamp precision: PRECISE (exact match required, no tolerance) |
| D6 | 1d historical fetch: NOT NEEDED (skip — trust local aggregation from 1m) |

### Final Friday LoC for heart piece: ~1,860 LoC

Updated API budget: 16% of daily quota (was 22% with 1d). Comfortable margin.

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 12:45 IST — EXTREME WORST-CASE SWEEP + Z+ UNIVERSAL ENFORCEMENT

Operator demand 2026-05-12 12:35 IST: "dig more worst cases dude"
Operator demand 2026-05-12 12:40 IST: "Z+ defensive layer always, every time, everywhere, every place, always dude"

**Locked `topic-extreme-worst-case-sweep.md`:**

110 NEW worst-case scenarios across 10 categories (W46-W155):
- A: AWS Infrastructure (W46-W60) — 15 scenarios
- B: Multi-System Cascade Failures (W61-W75) — 15 scenarios
- C: Indian Market Regulatory Events (W76-W90) — 15 scenarios
- D: Boot Sequence Partial Failures (W91-W100) — 10 scenarios
- E: Concurrency Races (W101-W110) — 10 scenarios
- F: Time/Clock Extremes (W111-W118) — 8 scenarios
- G: Storage/Disk Edge Cases (W119-W125) — 7 scenarios
- H: Security Attack Scenarios (W126-W135) — 10 scenarios
- I: Operator Error Scenarios (W136-W145) — 10 scenarios
- J: Truly Apocalyptic (W146-W155) — 10 scenarios

**GRAND TOTAL across all plan files: 366 defense paths.**

**APPENDIX A — Z+ UNIVERSAL ENFORCEMENT (locked FOREVER):**

Every one of the 366 scenarios subject to Z+ 7-layer pattern:
L1 DETECT, L2 VERIFY, L3 RECONCILE, L4 PREVENT, L5 AUDIT, L6 RECOVER, L7 COOLDOWN.

Plus 15-row × 7-layer = 105-cell matrix per item (operator-charter-forever §C):
code coverage / audit / testing / checks / performance / monitoring /
logging / alerting / security / hardening / bugs / scenarios /
functionalities / code review / extreme check.

Plus auto-driver test (universal): plain English, no library names,
no file paths, juice-shop/phone-line/bodyguard analogies, ASCII visuals.

Plus 4-word test: common / runtime / dynamic / scalable / incremental.

13 mechanical gates active and enforced:
- per-item-guarantee-check.sh
- banned-pattern-scanner.sh
- pub-fn-test-guard.sh
- pub-fn-wiring-guard.sh
- plan-verify.sh
- error_code_tag_guard.rs
- error_code_rule_file_crossref.rs
- dedup_segment_meta_guard.rs
- operator_health_dashboard_guard.rs
- resilience_sla_alert_guard.rs
- error_level_meta_guard.rs
- bench-gate.sh
- coverage-gate.sh

If you don't pass these gates, your PR doesn't merge. Period.

Honest envelope wording (universal mandatory):
> "100% inside the tested envelope, with ratcheted regression coverage:
>  366 worst-case scenarios mapped to L1-L7 defense layers,
>  5-tier persistence (rescue ring + spill + DLQ + shadow tables + ws_wal),
>  Bounded recovery times per scenario,
>  Beyond envelope: NDJSON DLQ + ws_wal/ raw frames + S3 5y retention,
>  Mathematically impossible literal: SEBI 24h JWT mandates >=1 disconnect/day."

Anything stronger without envelope qualifier = REJECT IN REVIEW.

**10 most underestimated scenarios** identified for special attention:
W47 (AWS EIP misclick), W57 (cost drift), W76 (NSE circuit breaker),
W81 (F&O ban list), W88 (bhavcopy retirement), W89 (margin change),
W136 (wrong config to AWS), W143 (operator restart mid-fetch),
W145 (forgot static IP verify), W153 (5-way simultaneous failure).

**Defense gaps surfaced for Friday:**
- AWS CloudTrail integration
- SMS backup contact (W154)
- live_instance_lock table verification (Wave-4 RESILIENCE-01)
- Daily AWS cost scrape
- F&O ban list daily ingest
- expected_candle_count per holiday in trading_calendar

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 13:05 IST — CROSS-VERIFY OBSERVABILITY + CLAUDE HANDOFF design

Operator demand 2026-05-12 13:00 IST: "how will you track + monitor + log +
audit + capture + notify me so I can hand instantly to a new Claude session?"

**Locked `topic-cross-verify-observability-and-claude-handoff.md`:**

The 5 W's of observability designed:
- TRACK: historical_fetch_state + cross_verify_state tables (live progress)
- MONITOR: 8 Prom metrics + 4 alerts + 6 Grafana panels
- LOG: tracing ERROR with code field → errors.jsonl
- AUDIT: cross_verify_mismatches + summary + NEW claude_handoff_audit tables
- CAPTURE: raw Dhan responses → data/captures/historical-fetch-<date>.jsonl.gz

THE KILLER FEATURE — Claude Handoff Bundle:
- File: data/claude-handoffs/cross-verify-<date>-FAILED.md
- 10 sections: mismatches / raw responses / SQL / grep / ratchets /
  commits / error code / env snapshot / investigation order / MCP refs
- Telegram contains ONE path; operator opens new Claude session +
  types "@<path> investigate" — total operator effort 30 seconds

Workflow:
T+0 incident → T+5s bundle written + Telegram sent →
T+30s operator opens Claude → T+5min fix shipped + ratchet added

15 worst-case scenarios for the handoff itself (W156-W170) all defended.

NEW audit table: claude_handoff_audit (SEBI 5y, tracks every incident
+ resolution).

Z+ 7-layer applied to the handoff system itself.

Friday LoC estimate for handoff system: ~940 LoC.

6 D-items pending operator decision (D1-D6 in plan).

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 13:30 IST — PER-CANDLE FIELDS + RAM DESIGN LOCKED (Claude-recommended)

Operator asked Claude to pick best approach: "when nanoseconds matter, RAM is the only choice".

Locked `topic-per-candle-fields-ram-design-LOCKED.md`:

**Q1 — Session start:** 09:15:00 IST sharp (NSE official)
**Q2 — Pre-open ticks:** EXCLUDED from session_open price; INCLUDED in volume_cumulative
**Q3 — IDX_I volume:** store 0; segment-check in code; frontend hides
**Q4 — Pct formula:** standard `(curr - prev) / prev × 100`; 0% on div-by-zero
**Q5 — Flush interval:** IMMEDIATE on bucket seal (async ILP)

**Memory design — HYBRID (best of both):**

| Tier | Purpose | Size | Pct computation |
|---|---|---|---|
| Tier 1 — Hot-path "current state" | Strategy reads | ~5 MB | PRE-COMPUTED (50ns reads) |
| Tier 2 — Sealed bar cache | Audit/replay | ~430 MB | Lazy on read (~10ns) |
| Indicator state (existing) | RSI/MACD/BB | 50 MB | — |
| Reference caches | prev_day_*, session_open | 5 MB | Static |
| **TOTAL** | | **~490 MB** | INSIDE 2 GB cap |

**Tier 1 — CurrentBarState struct (80 bytes, cache-aligned):**
- live OHLCV+OI (updated every tick)
- prev_day_close, prev_day_oi (from bhavcopy at 08:30)
- session_open_today_price/oi/volume_cum (captured at first tick after 09:15)
- 4 pre-computed pcts (close from prev_day, close from session_open,
  oi from prev_day, oi from session_open) — recomputed on every tick
- last_sealed_ts/close/oi/volume_bar (for indicator lookback)

**Tier 2 — SealedBarCompact (32 bytes):**
- ts, OHLC, volume_bar, oi_close
- 6.7M bars/day × 32B × 2 days = 428 MB

**Hot path budget:** ≤500ns per tick across all 6 TFs.
- HashMap lookup: ~20ns
- Struct update: ~20ns
- Pct recompute (4 divisions): ~20ns
- Bucket seal check: ~10ns
- × 6 TFs = ~420ns. Inside budget.

**Z+ 7-Layer applied to bar cache:**
L1 cache size gauge, L2 Tier1 vs Tier2 consistency check on seal,
L3 daily reconcile cache vs candles_1m_shadow, L4 boot-time
allocation with_capacity, L5 bar_cache_audit table, L6 rehydrate
from QuestDB on corruption, L7 5s exponential backoff.

5 NEW Prometheus metrics.

Hot path STAYS in nanosecond-land. DB is async cold path only.

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 13:50 IST — RAM REDESIGN: FULL 2 GB USED MASSIVELY

Operator clarification 2026-05-12 13:45 IST: "use 2GB massively for
future indicators + strategies, common runtime dynamic scalable."

Prior 490 MB design was too conservative. Wasted 1.5 GB headroom.

**REVISED 2 GB budget (operator-aligned):**

| Slot | MB | Notes |
|---|---|---|
| Rescue ring (5M × 200B) | 1024 | Zero-tick-loss — DO NOT TRIM |
| Tier 1 hot-path CurrentBarState | 5 | 50ns reads |
| Tier 2 today (compact 32B) | 215 | Today's audit + lookback |
| Tier 2 yesterday LRU | 50 | Lazy from disk on demand |
| **Indicator state (CONFIGURABLE)** | **230** | 6+ indicators × multi-period × 6 TFs |
| **Strategy state (CONFIGURABLE)** | **100** | Per-strategy × inst × TF |
| **Backtest warmup ring** | **50** | Cold-start bars cache |
| Greeks state | 50 | Existing |
| Depth book state | 20 | Bid/ask reconstruction |
| SPSC channels | 65 | Bounded mpsc |
| Token + cache | 10 | |
| WS buffers | 20 | |
| OMS queues | 10 | |
| Tracing queues | 5 | |
| Tokio runtime | 20 | |
| **SUBTOTAL** | **1874** | |
| Heap frag (~7%) | 130 | jemalloc |
| **TOTAL** | **~2004 MB** | INSIDE 2 GB cap ✅ |
| Margin | ~20 MB | Tight but workable |

**Key change vs prior 490 MB design:**
- Lazy-load yesterday's Tier 2 (saves 215 MB)
- Indicator state expanded 50 MB → 230 MB (4.6x growth)
- NEW strategy state slot: 100 MB
- NEW backtest warmup slot: 50 MB

**Common runtime dynamic scalable enforcement:**
- Indicators/strategies registered in config TOML
- Boot-time RAM budget guard validates sum vs INDICATOR_RAM_BUDGET_MB=230
  and STRATEGY_RAM_BUDGET_MB=100
- Boot FAILS on overflow with explicit "drop X or Y"
- Hot-reload on config change (existing notify watcher)
- Same TOML Mac dev = AWS prod

**Z+ 7-Layer for indicator/strategy RAM:**
- L1 tv_indicator_ram_bytes gauge per indicator
- L2 boot-time budget validate halt
- L3 daily reconcile enabled-config vs running-tasks
- L4 compile-time static_assert per indicator bytes
- L5 NEW indicator_lifecycle_audit table
- L6 fall back to prior config on reload failure
- L7 60s cooldown between config reloads

**What this unlocks for the future:**
1. Backtest 10+ indicators (single config flag)
2. Strategy library scales (per-strategy budget)
3. Per-TF independent state (each TF gets its own indicator instance)
4. Hot-reload without restart
5. Explicit budget enforcement (boot guard)

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 14:05 IST — BACKTESTING CORRECTION (tickvault is LIVE-ONLY)

Operator correction 2026-05-12 14:00 IST: "we won't do any backtesting
here — that's a SEPARATE product. When we finalize strategies +
indicators + technical + functional + percentage checks (in that
separate product), THEN we will implement those alone here in live."

**What changed:**
- DROP 50 MB "Backtest warmup ring" — not needed in live-only product
- Indicator cold-start (mid-day boot lookback) uses Tier 2 sealed bars
  (already allocated 215 MB today + 50 MB yesterday LRU)
- Multi-day lookback indicators (SMA-200 on 1d) lazy-load older days
  from QuestDB ONE TIME at boot, then run in-memory forever

**Reallocation:**
- 50 MB freed from removed warmup ring
- → Moved to indicator state: 230 MB → 280 MB (4.6x prior allocation)
- Total still 2.0 GB inside cap

**What tickvault DOES:**
- Run finalized indicators in live market
- Run finalized strategies (dry-run or live)
- Cross-verify post-market
- Audit + Telegram + Grafana

**What tickvault DOES NOT do (separate product):**
- Strategy backtesting
- Indicator parameter sweep
- Walk-forward analysis
- Monte Carlo simulation
- Strategy library design + iteration

**Common runtime dynamic scalable in LIVE-ONLY context:**
- Same TOML config Mac dev = AWS prod (common)
- Config hot-reload on indicators.toml change (dynamic)
- Boot guard validates RAM fit when new indicator added (scalable)
- Enable 1 indicator at a time post-backtesting (incremental)

Final budget unchanged at ~2 GB, with indicator slot bumped to 280 MB.

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 14:15 IST — AWS BUDGET + MAC↔AWS PARITY (common runtime)

Operator demand 2026-05-12 14:10 IST: comprehensive AWS budget +
monitoring + triggering + logging + tracking + capturing + finalized
automation. Plus Mac dev must replicate AWS (including core pinning).

Locked `topic-aws-budget-and-mac-aws-parity.md`:

**AWS Budget Automation (₹5K/mo cap):**
- Daily Cost Explorer scrape at 09:00 IST
- 6 thresholds: 70% Low / 90% High / 100% Critical / forecast / anomaly / 3x spike
- Telegram daily digest with breakdown
- NEW Lambda: instance-watchdog (5-min cron during business hours)
- NEW components: aws_budget_monitor.rs + aws_budget_audit table +
  AwsBudgetDigest notification

**Mac↔AWS Parity (10 checks):**
1. Docker compose SHA256
2. Config base.toml SHA256
3. Config indicators.toml SHA256
4. Config strategies.toml SHA256
5. Docker image digest match
6. Rust binary git commit hash
7. QuestDB schema match
8. Grafana dashboards SHA256
9. Prometheus alerts.yml SHA256
10. Core pinning status (Mac=4/4, AWS=4/4)

NEW: `make parity-check` command
NEW: parity_check_audit table (daily 23:00 IST + boot 08:30)
NEW: parity_drift_count gauge per check_name
NEW: ParityDrift Telegram event

**Core Pinning Parity (Wave 5 CORE-PIN-01):**
- 4 Tokio worker threads pinned to cores 0-3
- Same `core_affinity` crate works on macOS + Linux
- Mac M1/M2: P-cores 0-3
- AWS c8g.xlarge: vCPUs 0-3
- Drift watchdog re-pins on detection
- Verified via tv_core_pinning_workers_pinned_total gauge

**Comprehensive automation chain (5 W's both machines):**
- TRACK / MONITOR / LOG / AUDIT / CAPTURE all on both Mac dev + AWS prod
- Same code, different SSM env paths, different Telegram channels
- Same Prometheus / Grafana / QuestDB schema

**Critical invariant:** Mac dev and AWS prod ALWAYS on same git commit
during trading hours. Parity check step 6 enforces.

10 NEW worst-case scenarios (W171-W180) for parity drift defended.

Friday LoC estimate: ~1,440 LoC.

5 D-items pending operator decision.

Discussion mode continues. NO IMPLEMENTATION.

## 2026-05-12 15:35 IST — COMPREHENSIVE TIMING AUDIT (Option A swept)

Operator: "Yes check Option A dude" — audit all plan files for timing bugs.

Locked `topic-comprehensive-timing-audit-fix.md`:

**23 timing violations found across 10+ plan files:**

Category A — 23:00 IST violations (15 instances):
- topic-questdb-7-layer-z-plus-defense.md line 75
- topic-tick-to-candle-math-prevday-sourcing.md lines 173, 278, 390, 671
- topic-zero-tick-loss-coverage-map.md line 242
- topic-observability-7-layer-z-plus-defense.md lines 89-92 (4 reconciles)
- topic-z-plus-retrofit-23-monday-tasks.md line 86
- topic-memory-wal-ring-shadow-deep-drill.md (multiple L3 cells)

Category B — 08:15 IST bhavcopy violations (6 instances):
- topic-tick-to-candle-math-prevday-sourcing.md lines 263, 451, 539
- topic-z-plus-retrofit-23-monday-tasks.md line 164
- topic-post-market-historical-fetch-cross-verify.md lines 43, 57
- topic-cross-verify-observability-and-claude-handoff.md (auto-driver story)

Category C — Midnight rollover (3 instances):
- 99-mega-plan-strawman.md (multiple)
- topic-tick-to-candle-math-prevday-sourcing.md
- topic-zero-tick-loss-coverage-map.md

Category D — Nightly EBS snapshot: FALSE alarm (AWS Backup is server-side)

**Fix matrix:**
- 23:00 IST daily reconciles → 17:25 IST (pre-shutdown)
- 08:15 IST bhavcopy → 08:35 IST (post-boot orchestrator)
- 00:00 IST midnight rollover → 08:30:30 IST "new-day boot logic"

**Revised daily schedule (operator-corrected):**

08:30:00  AWS auto-start
08:30:30  New-day boot logic (drop caches, reset counters)
08:35:00  Bhavcopy download (prev_day_oi/high/low/close)
08:35:30  Parity check #1 vs origin/main
08:45:00  Pre-market token validity
09:00:00  AWS budget scrape
09:13:00  Phase 2 readiness
09:15:00  Market open
09:15:30  Streaming confirmation
09:16:30  Market-open self-test
... TRADING SESSION ...
15:30:00  Market close
15:31:00  Post-market historical fetch
16:08:00  Cross-verify
16:15:00  Telegram digest
16:20:00  Indicator snapshots saved
17:25:00  DAILY RECONCILES (was 23:00)
17:28:00  Telegram daily digest
17:30:00  AWS auto-stop (hard kill)

6 NEW worst-case scenarios W185-W190 added.

**Grand total worst-case paths across all plans: 401.**

**Plan files needing edits in Friday work: 8 files, ~30 line changes total.**

Discussion mode continues. NO IMPLEMENTATION.

2026-05-13 ~10:30 IST — PHASE 0 LEAN PLAN LOCKED
================================================
Operator decision after live disconnect storm 09:16-09:29 IST:
- Scope shrinks from full universe (10K instruments, 11 WS conns) to lean MVP (~221 instruments, 2 WS conns)
- Only 3 indices + 218 F&O underlying stocks in Ticker mode on 1 main-feed WS conn
- Order Update WS stays (already separate endpoint)
- Depth-20, Depth-200, Greeks pipeline, Movers pipeline, dynamic selectors, Phase 2 dispatcher, depth rebalancer — ALL PARKED to Phase 2 via feature flags
- AWS tier drops from c7i.xlarge (8GB / ~Rs 3530/mo) to t3.medium (4GB / ~Rs 700/mo) — total infra ~Rs 1252/mo (75% under Rs 5K budget)
- Mac dev + Mac backtest (Dhan historical alone, no Groww needed)
- Friday 2026-05-15 build: 3 subscription/conn changes + 5 hardening changes from disconnect-storm analysis = ~620 LoC
- 6-week path: build (Fri) → backtest validate (weekend) → AWS deploy (Mon) → Phase 1 dry_run 22 trading days → Phase 2A 1 strategy live 2 weeks → Phase 2B feature re-enable per data
- All other 35+ topic files in friday-may-15-mega/ are now Phase 2 reference material; LOCKED file is self-contained for Friday build
- Canonical file: topic-PHASE-0-LEAN-LOCKED.md


## 2026-05-13 (afternoon) — Disconnect gap-fill + dual-gate market-hours fix

- REJECT `/marketfeed/ltp` for gap-fill (no per-minute OHLCV, no proper timestamp). REPLACE with `/v2/charts/intraday` seal-then-fetch (bar_end + 5s buffer, DEDUP UPSERT, multi-minute support, 15:30 cutoff).
- 7-layer observability mandated for every disconnect/reconnect/resubscribe/backfill event: 5 Prom counters + 2 gauges + ws_reconnect_audit + gap_fill_audit + 5 typed Telegram variants + Grafana + 6 ratchet tests.
- BUG locked: 15:29:59.586 skipped-tick — market-hours gate uses local `now()` instead of `tick.exchange_timestamp_ist`. The 500-600ms pipeline delay pushed local clock past 15:30:00 → tick rejected even though exchange_ts < 15:30.
- FIX: dual-gate design. G1 (Exchange Gate) gates on `tick.exchange_timestamp_ist`, range `[09:15:00.000, 15:30:00.000)` exclusive on close. G2 (Wall-Clock Gate) keeps socket open + bar-seal pending until 15:31:00 IST (60s grace after market close). Final 15:29 bar seals at 15:31:00.
- Banned-pattern hook category added: any `is_within_market_hours.*now\(\)` → REJECT at commit.
- Phase 0 Friday build revised: ~1,330 LoC across 14 changes (was ~620 LoC across 8).
- Post-15:30 cross-verify (live vs Dhan historical, zero tolerance) is unchanged — existing `historical-candles-cross-verify.md` rule continues to apply, runs at 15:31:05 IST after the final bar seals.

## 2026-05-13 (evening) — Pre-open equilibrium = 09:15 candle open

- BUG locked: we currently treat first WS tick after 09:15:00 as the OPEN of the 09:15 1m candle. WRONG. NSE's OFFICIAL OPEN is the pre-open call-auction equilibrium price FROZEN at 09:08:00 IST. The first post-open trade is just the first post-open trade, not the OPEN.
- IMPACT: ₹2-5 silent drift per stock per day on the daily candle OPEN. Compounds in backtests. Gap-up/gap-down signals differ from NSE truth.
- FIX: aggregator initializes 09:15 candle.open from preopen_buffer last slot for IDX_I NIFTY/BANKNIFTY + NSE_EQ 218 F&O stocks. SENSEX (BSE) uses first tick + cross-verify flag. VIX (no pre-open) uses first tick. NSE_EQ fallback chain: buffer → REST `/v2/marketfeed/quote.day_open` (at 09:14:55) → first WS tick + warn.
- INVARIANT: after 09:15:00.000, OPEN field is FROZEN. Ticks only update HIGH/LOW/CLOSE/VOLUME.
- CROSS-CHECK: at 09:16:05 IST, fetch Dhan `/v2/charts/intraday` 09:15 bar across all 222 SIDs and compare our.open vs dhan.open. Mismatch fires `OpenPriceMismatchVsDhan` (Critical).
- AUDIT: new `open_price_audit` table — per-SID daily row with `(our_open, dhan_open, source, mismatch_pct, result)`. DEDUP UPSERT KEYS `(trading_date_ist, security_id, exchange_segment)`.
- 3 new typed Telegram events + 8 ratchet tests.
- Phase 0 Friday build revised: ~2,030 LoC across 18 changes (was ~1,330 LoC across 14).

## 2026-05-13 (night) — Tightened timings + 12 missed P0 items + GIFT NIFTY

### Tightened timings (item 10 in plan)
- First reconnect attempt: 0ms (instant)
- IDX_I activity watchdog: 3s (was 15s)
- NSE_EQ activity watchdog: 10s (was 30s)
- VIX activity watchdog: 30s
- Subscribe-ACK first frame: 2s (was 5s)
- WS handshake deadline: 5s
- Order placement REST timeout: 5s

### 12 P0 items previously missed (items 11-21 in plan)
- 11: SEBI 24h JWT daily renewal observability + auth_renewal_audit table + JwtRenewalCompleted Telegram (Info)
- 12: Static IP boot check via /v2/ip/getIP, verify ordersAllowed=true, HALT if false
- 13: Dual-instance lock (RESILIENCE-01) via QuestDB live_instance_lock table; refuse boot if another instance < 60s old
- 14: Orphan position 15:25 IST watchdog — auto-close any open positions 5min before market close
- 15: NaN / division-by-zero guards in indicator engine — wrap every indicator update, reset state on NaN, Telegram alert
- 16: Order placement 5s REST timeout + DH-904 exponential backoff + DH-905/906 never-retry policy
- 17: Stop-loss-leg-cancelled detection — order-update WS Source=N+LegNo=2+status=CANCELLED → auto-place fresh SL within 30s
- 18: Boot-success Telegram positive ping at 09:14:55 IST (BootReadyConfirmation Info / BootDegraded Critical)
- 19: End-of-day Telegram digest at 15:31:30 IST — P&L, signals, fills, disconnects, gap-fills, cross-verify result
- 20: Tick-level integrity guards — reject price<=0, price>10M, oi<0; aggregator high>=low && open/close in [low,high] at seal
- 21: Self-trade prevention — refuse SELL within 60s of BUY on same SID+ProductType (SEBI wash-trade rule)

### GIFT NIFTY (item 22, with 7 sub-items)
- GIFT NIFTY = NIFTY 50 futures on NSE IX (GIFT City), USD-denominated, ~21h/day across 2 sessions
- Operator chose Option B: extend AWS to 06:30-15:45 IST Mon-Fri (captures Session 1 + overnight close via REST at boot)
- Per-segment market-hours table replaces single [09:15, 15:30) constant
- NSE_IFSC segment added to ExchangeSegment enum (pending Dhan support verification)
- AWS cost delta: +~₹20/mo (still under ₹5K cap)
- Pre-flight verification mandatory before Friday: confirm Dhan supports GIFT NIFTY via WS + /charts/intraday; operator to grep instrument master CSV on Thu 2026-05-14

### Phase 0 LOCKED total
- 22 top-level items (with sub-items in #22) = ~4,120 LoC
- 1.5-2 focused days (Friday + Saturday morning)
- AWS cost: ~₹1,275/mo (75% under budget)

## 2026-05-13 (final session) — Prev-close mode mix + Phase 0 FINALIZED

### Bug found and fixed (item 23)
- Ticker-only mode CANNOT fetch prev_close for NSE_EQ stocks. Code 6 PrevClose packet is IDX_I-only. NSE_EQ requires Quote (bytes 38-41) or Full (bytes 50-53).
- Mixed-mode locked: IDX_I=Ticker (gets code 6), NSE_EQ 218 stocks=Quote (gets bytes 38-41), GIFT NIFTY=Ticker (pending).
- Bandwidth: ~250 MB/day vs ~78 MB/day. Trivial on t3.medium.
- Belt-and-suspenders: REST /v2/charts/historical interval=1d fetch at 06:30 IST boot for all 222 SIDs. Populates RAM cache BEFORE market opens.
- PrevCloseMissingAtMarketOpen Critical Telegram at 09:14:55 if any SID lacks data.
- Banned-pattern hook: Ticker subscription on NSE_EQ → REJECT at commit.

### Phase 0 LOCKED FINAL TALLY
- Top-level items: 23
- Total LoC: ~4,750
- Friday + Saturday: ~2 focused days
- Total SIDs: 223 (222 NSE/IDX_I + 1 GIFT NIFTY pending)
- Mode mix: IDX_I Ticker + NSE_EQ Quote + GIFT Ticker
- AWS cost: ~₹1,275/mo (75% under ₹5K cap)
- AWS schedule: 06:30-15:45 IST Mon-Fri (revised for GIFT NIFTY)
- Branch: claude/trading-tick-vault-BkvpS
- Phase 0 LOCKED is COMPLETE pending operator pre-flight verification on Thu 2026-05-14:
  1. GIFT NIFTY in Dhan instrument master CSV
  2. SENSEX code 6 packet behavior
  3. NSE_EQ Quote mode bytes 38-41 prev_close verification
  4. /v2/charts/historical interval=1d format verification

## 2026-05-13 (final-final) — GIFT NIFTY DROPPED from Phase 0

- Operator decision: drop GIFT NIFTY from Phase 0 to keep Friday build lean.
- Reasons: Dhan support unverified (adds Thu pre-flight risk), per-segment-hours table adds ~150 LoC complexity, AWS schedule revision (08:30→06:30) was reversible noise, overnight gap-direction signal can be inferred from NIFTY morning behaviour during Phase 1.
- Reverted: AWS schedule back to 08:30-17:30 IST Mon-Fri (original). Single market-hours window [09:15, 15:30) restored (no per-segment table). NSE_IFSC segment NOT added.
- Total SIDs: 222 (was 223). LoC: ~4,170 (was ~4,750).
- GIFT NIFTY design remains documented in plan file as PARKED (item 22-OLD) for Phase 2 re-evaluation.
- Re-eval trigger: Phase 1 monitoring data shows clear demand for overnight gap signal.
- Phase 0 LOCKED is now 22 items (1-21 + 23, where 23 is prev-close mode mix), 4,170 LoC, 1.5-2 days.
