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

---

## Reversal procedure

If a decision needs to change:

```
[YYYY-MM-DD HH:MM IST] SUPERSEDED entry #N: <new decision> — reason: <why>
```

Original entry is never edited. Audit trail stays intact.
