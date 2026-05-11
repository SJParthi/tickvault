# Step 1 — Mon Eve Discussion Log (Mon 2026-05-11)

**Topic:** WS Disconnect Resilience + Honest Guarantee Envelope
**Status:** WRAPPED for Mon eve — resumes Tue eve for Step 2 (capital + risk)
**Duration:** 18:30 IST → 21:30 IST (~3 hours across web + CLI)

---

## Timeline

| Time IST | Event | Outcome |
|---|---|---|
| 18:30 | 10 parallel specialist research agents spawned | Mega plan strawman synthesized → `99-mega-plan-strawman.md` |
| 19:00 | AskUserQuestion #2: scope of week | First live order DEFERRED to Wave 8. Shadow-equivalence max. |
| 19:30 | Workflow mode locked | FAANG-style design review meetings Mon/Tue/Wed, NOT one-shot |
| 19:40 | origin/main pulled in | 2 commits gained (PR #600 W7-A4.5 + PR #602 W7-A4.6) |
| 20:00 | Plan-shape SUPERSEDED | Hybrid Option D: multi-file directory `friday-may-15-mega/` |
| 20:25 | Write-Before-Reply protocol | Every reply writes `.last-cursor.md` + (if decision) appends log BEFORE chat reply |
| 20:35 | 3-layer recovery hardened | README + INDEX + log + cursor + `scripts/recover.sh` (Layer 3 transcript fallback) |
| 20:50 | Task Board pattern | `tasks/_board.md` for 5+ parallel Claude sessions Friday |
| 20:55 | **Honest envelope re-affirmed** | Literal "never disconnect / never lose tick" IMPOSSIBLE (SEBI 24h JWT + Dhan static IP forces ≥1 reconnect/day). Bounded zero-loss inside chaos envelope IS guaranteed. 22-scenario matrix enumerated. 8 gaps identified. |
| 20:58 | Full-mode confirmation | 11,034 instruments under Wave 5 `indices_only_all_expiries` scope. NOT a disconnect cause; can AMPLIFY backpressure only. Mac M4 Pro 48GB outspecs AWS c8g.xlarge. |
| 21:00 | **Phase 0.5 ADOPTED** into mega plan | 8 + 1 items closing 8 of 22 disconnect-cause gaps. 5 reserved ErrorCodes promoted + 3 new + 1 verification. |
| 21:02 | Mac dev env documented | MacBook Pro M4 Pro 14 cores (10P+4E) 48GB. Out-specs AWS c8g.xlarge. Network only AWS-improved variable. Common-runtime preserved. |
| 21:03 | **Core-pinning design LOCKED** | ONE dedicated core for ALL 5 WS conns (async-multiplex), NOT one core per conn. Same 4-core scheme Mac=AWS. Item 0.5.9 verifies main.rs wiring. |
| 21:05 | **Web session 400'd** | "API Error: 400 messages.150.content.2.text: cache_control cannot be set for empty text blocks" → retries continued failing → session broke |
| 21:08 | **CLI continuation begun** | Same branch `claude/trading-tick-vault-BkvpS`. All decisions persisted to disk. No context lost. |
| 21:10 | Honest envelope reference doc written | `step-1-honest-envelope.md` — canonical reference for any future session |
| 21:15 | **Item 0.5.10 ADDED** (operator pick A in web before 400) | Consolidated `PreMarketReady` Telegram replacing 6 fragmented messages. 200 LoC estimate. Task file `tasks/T05-10-*.md` deferred to Wed. |
| 21:20 | FAANG 3-day map LOCKED | Mon eve = Step 1 wrap ✅ / Tue eve = Step 2 (capital+risk) / Wed eve = Step 3 (go-live + adversarial + APPROVE) |
| 21:25 | Implementation rule LOCKED | NO implementation Mon-Wed. Pick C explicitly rejected. All execution Friday parallel sessions. |
| 21:30 | Operator confirmed charter | "extreme complete comprehensive extensive automation 100 percentage" + every-session-auto-everything + 15-row + 7-row matrix on every item |

---

## Key Q&A from operator (verbatim or paraphrased)

### Q1 (web, ~18:30): "Top priority for this week?"
**Decision:** All three phases in sequence — W6 close-out → W7-A4 wire-up → live trading go-live. Source: AskUserQuestion #1.

### Q2 (web, ~19:00): "First live order this week?"
**Decision:** DEFERRED to Wave 8 operator decision. This week's scope max = shadow-equivalence (paper-vs-live diff with `dry_run=true` still set).

### Q3 (web, ~19:30): "Plan format?"
**Decision:** FAANG-style design review meetings Mon/Tue/Wed evening, NOT one-shot plan approval. Operator brings thoughts, Claude pushes back / documents / explains kid-friendly. Approval flip on Wed.

### Q4 (web, ~20:55): "Can I guarantee zero tick loss + WS never disconnects?"
**Decision:** NO literally — bounded zero-loss inside chaos envelope per `wave-4-shared-preamble.md` §8. The 22-scenario disconnect matrix enumerated; 8 gaps identified; Phase 0.5 closes all 8.

### Q5 (web, ~21:00): "Pick A?" (consolidated Pre-Market-Ready Telegram)
**Decision:** YES — Item 0.5.10 added to Phase 0.5.

### Q6 (web ~21:02, web 400'd shortly after): "WS reconnect — is current approach right?"
**Decision:** YES — current approach is correct. Alternatives (overlap-swap, gold redundant 2× WS) burn Dhan-cap slots or weeks of work for ~5s/day savings. Bad ROI.

### Q7 (CLI, 21:25): "Should we check ALL picks A/B/C/D like a FAANG meeting?"
**Decision:** YES — every option evaluated, then converge. Tonight: A (defer to Wed), B (move to Tue), C (rejected — no implementation), D (re-asked guarantee → answered).

### Q8 (CLI, 21:30): "Confirm 100% charter applied to every plan item?"
**Decision:** YES — 15-row + 7-row matrix per `per-wave-guarantee-matrix.md`. Mechanically enforced by `per-item-guarantee-check.sh` + `make wave-guarantee-check`.

### Q9 (CLI, 21:35): "Persist everything to files NOW — don't worry about tokens"
**Decision:** YES — this discussion + future agendas + cursor + decisions log all written to `friday-may-15-mega/`. Future sessions resume from files, not chat.

---

## Picks evaluated tonight (FAANG style — all options weighed)

| Pick | Question | Verdict | Reason |
|---|---|---|---|
| A (web) | "YES add item 0.5.10 (PreMarketReady)" | ✅ ACCEPTED | Locked at 21:00 IST |
| A (CLI) | "Spawn 3 adversarial agents now" | 🟡 DEFERRED to Wed | Saves Mon/Tue tokens for ideation; charter §3 mandates pre-approval review |
| B | "Move to Step 2 now" | 🟡 DEFERRED to Tue | Out of scope for Mon eve |
| C | "Start implementing T05-01 now" | ❌ REJECTED | Operator explicit: NO implementation for 3 days |
| D | "Different angle" | 🎤 USED | Operator re-asked guarantee charter; answered |
| E | "End session tonight, resume Tue eve" | ✅ EXECUTING | After persistence pass |
| F | "One more Step 1 question tonight" | OPEN | Operator can use it before end-of-session |
| G | "Show me the strawman 99 file" | OPEN | Operator can read offline |

---

## Files modified Mon eve (commits)

| Commit | Branch | What |
|---|---|---|
| 4f97281 | `claude/trading-tick-vault-BkvpS` | `docs(plan): add Phase 0.5 (WS disconnect resilience) + Mac dev env to mega plan` — strawman extended with Phase 0.5 + Mac config |
| bf62c99 | `claude/trading-tick-vault-BkvpS` | `chore(plan): update cursor — CLI continuation after web 400'd session` |
| (this turn) | `claude/trading-tick-vault-BkvpS` | Step 1 honest envelope reference + discussion log + Step 2/3 agendas + cursor + decisions log + INDEX |

---

## Operator's Telegram screenshot proof (Mon 9:04 PM IST)

Live system streaming. Boot complete. 97,422 derivatives loaded. Auth OK. Order Update WS connected and reconnected once. NSE bhavcopy cross-check FAILED (questdb_query_failed — Phase 0.5 NET-02/DH-911 will catch upstream causes). Clean shutdown at 19:18 IST.

This screenshot is the PROOF the operator demanded:
- Every Telegram message tagged with severity ([LOW]/[MEDIUM]/[HIGH])
- Edge-triggered alerts (zero-disconnect swap mentioned — Wave 2 zero-disconnect rebalance working)
- Boot-complete confirmation
- Shutdown traceability
- Auth trail
- Cross-check audit (failed → operator sees within minutes)

**System IS real. System IS streaming. Phase 0.5 adds 8 more failure-mode detectors.**

---

## What's NOT done tonight (carried forward)

- Adversarial 3-agent review on Phase 0.5 design — DEFERRED to Wed
- Step 2 (capital + risk envelope) discussion — Tue eve
- Step 3 (go-live + APPROVE) discussion — Wed eve
- Task file content (T05-01 through T05-10) — Wed during sub-PR planning
- Friday Task Board fan-out — Wed night
- Mega plan REWRITE Status DRAFT → APPROVED — Wed night

---

## What ANY future session must do when opening cold

1. `git pull origin claude/trading-tick-vault-BkvpS`
2. Read `friday-may-15-mega/INDEX.md` → "where are we"
3. Read `friday-may-15-mega/step-1-honest-envelope.md` → CANONICAL reference (do NOT re-debate)
4. Read this file (Mon eve log) → full transcript summary
5. Read `friday-may-15-mega/.last-cursor.md` → in-flight state
6. Read `friday-may-15-mega/00-decisions-log.md` (tail -30) → recent locks
7. Read `friday-may-15-mega/step-2-tue-eve-agenda.md` OR `step-3-wed-eve-agenda.md` if appropriate
8. Ask operator: "I see we wrapped Step 1 Mon eve. Continuing with [Step 2 capital+risk / Step 3 go-live]?"

**No context lost. Web 400 cannot kill what's on disk.**
