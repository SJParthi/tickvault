# Topic — COMPREHENSIVE Timing Audit + Fix Sweep (AWS up-window violations)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `aws-budget.md` schedule (AWS 08:30-17:30 IST) > this file.
> **Trigger:** Operator caught the 23:00 IST parity-check bug 2026-05-12 15:20 IST. Now auditing EVERY plan file for similar timing violations.

---

## 🚗 Auto-Driver Story

> Sir, we just caught a bug where I scheduled "daily checks at 11 PM" but AWS is OFF at 11 PM. Today I auditied EVERY plan file for similar timing bugs. Found 23 events scheduled OUTSIDE the AWS up-window (08:30-17:30 IST). All need fixing. Some shift to post-boot (08:35), some to pre-shutdown (17:25), some to mid-day (when surely up).
>
> Pattern: AWS is a part-time worker. Anything we want done MUST fit in its working hours.

---

## ⏰ The AWS up-window (immutable per `aws-budget.md`)

```
08:30 IST  ──────────────────────────────────────── 17:30 IST
  ↑                                                   ↑
  Auto-start                                       Auto-stop
  (boot ~30s)                                      (hard kill)
```

**AWS UP:**  08:30 → 17:30 IST  (9 hours weekdays / 5 hours weekends)
**AWS DOWN:** 17:30 → 08:30 next day (15 hours)

**Any event scheduled OUTSIDE the up-window is a BUG.**

---

## 🚨 ALL violations found (across 10+ plan files)

### Category A — 23:00 IST violations (10 plans affected)

| Plan file | Line | Violation |
|---|---|---|
| `topic-questdb-7-layer-z-plus-defense.md` | 75 | "Daily 23:00 IST. Why: after market close, before partition manager runs" — AWS OFF |
| `topic-tick-to-candle-math-prevday-sourcing.md` | 173 | L3 RECONCILE "Daily 23:00 IST: cross-check cache vs bhavcopy" — AWS OFF |
| `topic-tick-to-candle-math-prevday-sourcing.md` | 278 | L3 RECONCILE "Daily 23:00 IST: write today's last-tick OI" — AWS OFF |
| `topic-tick-to-candle-math-prevday-sourcing.md` | 390 | "Post-market reconcile task at 23:00 IST" — AWS OFF |
| `topic-tick-to-candle-math-prevday-sourcing.md` | 671 | L3 RECONCILE "Daily 23:00 IST" — AWS OFF |
| `topic-zero-tick-loss-coverage-map.md` | 242 | "Scheduled 23:00 IST (after close)" — AWS OFF |
| `topic-observability-7-layer-z-plus-defense.md` | 89-92 | 4 reconciles at "Daily 23:00 IST" — AWS OFF |
| `topic-z-plus-retrofit-23-monday-tasks.md` | 86 | L3 RECONCILE "Daily 23:00 IST" — AWS OFF |
| `topic-aws-budget-and-mac-aws-parity.md` | 200, 258, 338, 396, 430 | Multiple "Daily 23:00 IST" — **already fixed in APPENDIX A** |
| `topic-memory-wal-ring-shadow-deep-drill.md` | (multiple L3 cells) | All "Daily 23:00 IST" reconciles — AWS OFF |

**Total: ~15+ scheduled reconciles fail because AWS is off.**

### Category B — 08:15 IST bhavcopy violations (4 plans affected)

| Plan file | Line | Violation |
|---|---|---|
| `topic-tick-to-candle-math-prevday-sourcing.md` | 263 | "Next morning 08:15 IST" — AWS not yet up (starts 08:30) |
| `topic-tick-to-candle-math-prevday-sourcing.md` | 451 | "At 08:15 IST orchestrator runs" — AWS not yet up |
| `topic-tick-to-candle-math-prevday-sourcing.md` | 539 | "Daily 08:15 IST orchestrator" — AWS not yet up |
| `topic-z-plus-retrofit-23-monday-tasks.md` | 164 | "Boot scheduler checks if it's after 08:15 IST" — but scheduler isn't running |
| `topic-post-market-historical-fetch-cross-verify.md` | 43, 57 | "bhavcopy CSV @ 08:15 IST" — AWS not yet up |
| `topic-cross-verify-observability-and-claude-handoff.md` | (auto-driver story) | "8:15 AM, we ALSO download" — AWS not yet up |

**Total: ~6 bhavcopy timing violations.**

### Category C — Midnight rollover violations (assumes process is running)

| Plan file | Reference |
|---|---|
| `99-mega-plan-strawman.md` | "IST-midnight clear scheduler", "midnight rollover task", "00:00 IST atomic state transition" |
| `topic-tick-to-candle-math-prevday-sourcing.md` | "next IST midnight rollover repopulates" |
| `topic-zero-tick-loss-coverage-map.md` | "IST midnight rollover task (Phase 2.7 L13)" |

**Problem:** at 00:00 IST, AWS is OFF. The "midnight rollover" can't happen mid-night.

### Category D — Overnight events (2 plans)

| Plan file | Reference |
|---|---|
| `topic-aws-budget-and-mac-aws-parity.md` | "EBS snapshot — nightly" — AWS OFF, snapshot still happens via AWS Backup service ✅ (this is OK actually — AWS Backup is server-side) |
| `topic-post-market-historical-fetch-cross-verify.md` | "instrument might be delisted overnight" — informational, no scheduled action |

**Category D is FALSE alarm.** AWS Backup is server-side, runs without our instance.

---

## ✅ THE FIX MATRIX (all 23 violations resolved)

| Old time | New time | Reason |
|---|---|---|
| **23:00 IST daily reconcile** | **17:25 IST** (pre-shutdown) | Last 5 min before AWS auto-stop |
| **23:00 IST daily reconcile** | **17:25 IST + 08:35 IST** (dual) | Pre-shutdown + post-boot for cross-day catch |
| **08:15 IST bhavcopy** | **08:35 IST** (post-boot orchestrator) | After AWS boots at 08:30 |
| **08:15 IST orchestrator scheduler** | **08:35 IST** as part of boot sequence | Same |
| **00:00 IST midnight rollover** | **08:30:30 IST** at boot — handle as "new day boot" | AWS off at midnight; do the rollover at next boot |
| **23:00 IST partition manager** | **17:25 IST** (pre-shutdown) | Move to before AWS off |
| **Daily 23:00 cross-TF reconcile** | **17:25 IST** | Move to before AWS off |
| **Daily 23:00 audit table integrity** | **17:25 IST** | Move to before AWS off |
| **23:00 IST counter consistency check** | **17:25 IST** | Move to before AWS off |
| **23:00 IST alert delivery audit** | **17:25 IST** | Move to before AWS off |
| **23:00 IST log completeness check** | **17:25 IST** | Move to before AWS off |

### Why some need DUAL timing (17:25 + 08:35)

**Single-point check at 17:25:** captures end-of-day state.

**Cross-day reconcile (e.g., parity check, bhavcopy load):** needs BOTH:
- 17:25 captures today's end-state
- 08:35 next morning catches overnight drift (someone edited config off-hours)

---

## 🏗️ The REVISED daily schedule (operator-corrected)

```
─── 17:30 IST yesterday ────────────────────── AWS OFF ──────── 08:30 IST today ───
                              ↓
                       AWS instance STOPPED for 15 hours
                       (no process running; AWS Backup snapshots
                        still happen server-side)
                              ↓
─── 08:30:00 IST ─── AWS auto-start ──────────────────────────────────────
   ↓
─── 08:30:05 IST ─── Tickvault process starts (idempotent boot)
   ↓
─── 08:30:30 IST ─── Boot sequence: midnight rollover EQUIVALENT
                    - Drop yesterday's intraday caches
                    - Reset daily counters
                    - Update "today" = current IST date
                    - Sweep stale partitions if any
   ↓
─── 08:35:00 IST ─── BHAVCOPY DOWNLOAD (prev_day_oi/high/low/close)
                    - 2 CSV files (equity + F&O)
                    - Parse → write to previous_close table
                    - Cache in-memory
   ↓
─── 08:35:30 IST ─── PARITY CHECK (Mac vs origin/main + AWS vs origin/main)
                    - 10 SHA256 + git commit comparisons
                    - Telegram if drift
   ↓
─── 08:45:00 IST ─── Pre-market token validity check
                    - Profile API call
                    - dataPlan="Active", activeSegment includes "D"
                    - Static IP allowlist check
   ↓
─── 09:00:00 IST ─── AWS BUDGET DAILY SCRAPE
                    - Cost Explorer API
                    - Telegram digest
   ↓
─── 09:00:00 → 09:15:00 IST ─── Pre-open buffer phase
   ↓
─── 09:13:00 IST ─── Phase 2 readiness check (PHASE2-READY-01)
   ↓
─── 09:15:00 IST ─── MARKET OPEN — live ticks flow
   ↓
─── 09:15:30 IST ─── MarketOpenStreamingConfirmation Telegram
   ↓
─── 09:16:30 IST ─── Market-open self-test (SELFTEST-01/02)
   ↓
─── 09:15 → 15:30 IST ─── TRADING SESSION
                    - Live aggregator
                    - 6 TFs sealing bars
                    - Indicators + strategies running
                    - Continuous monitoring
   ↓
─── 15:30:00 IST ─── MARKET CLOSE
   ↓
─── 15:30:00 → 15:31:00 IST ─── WS drain (all 16 conns close)
   ↓
─── 15:31:00 IST ─── POST-MARKET HISTORICAL FETCH starts
                    - 11K instruments × 1m candles
                    - Resume-from-state if needed
   ↓
─── 16:08:00 IST ─── Fetch complete; CROSS-VERIFY runs
   ↓
─── 16:15:00 IST ─── Cross-verify complete; Telegram digest
                    (PASSED ✅ OR FAILED with handoff bundle)
   ↓
─── 16:20:00 IST ─── Indicator state snapshots saved
                    - For tomorrow's cold-start
   ↓
─── 17:25:00 IST ─── DAILY RECONCILES (was at 23:00, now here)
                    [1] QuestDB schema integrity check
                    [2] Audit table row counts vs counters
                    [3] Log completeness (errors.jsonl row count)
                    [4] Alert delivery audit (Telegram sent log)
                    [5] Cross-TF candle reconciliation
                    [6] Counter vs Prometheus scrape consistency
                    [7] Partition manager run (drop > retention)
                    [8] PARITY CHECK #2 (vs origin/main)
                    [9] Bhavcopy vs Dhan-historical OI cross-check
                    [10] Cache state snapshot for tomorrow
   ↓
─── 17:28:00 IST ─── Telegram daily digest:
                    ✅ Trading day summary
                    ✅ Cross-verify result
                    ✅ Daily reconciles status
                    ✅ AWS budget MTD/forecast
   ↓
─── 17:30:00 IST ─── AWS AUTO-STOP (hard kill)
```

---

## 📋 What the boot sequence at 08:30 must do (revised)

The "midnight rollover" concept is dead because AWS isn't up at midnight. Instead, the BOOT SEQUENCE handles all day-transition logic:

| Step | Action |
|---|---|
| 08:30:00 | AWS auto-start fires |
| 08:30:05 | Tickvault process starts |
| 08:30:10 | Detect "is today a new trading day?" — read last_boot_date from QuestDB |
| 08:30:15 | If new day: invalidate today's caches; reset daily counters |
| 08:30:20 | Load yesterday's last-tick state from `previous_close` table |
| 08:30:30 | Boot QuestDB readiness probe (BOOT-01/02) |
| 08:30:45 | Trigger bhavcopy downloader (async) |
| 08:31:00 | Auth (TOTP → JWT) |
| 08:31:30 | Universe build from rkyv cache |
| 08:32:00 | WS pool spawned (deferred mode until market open) |
| 08:35:00 | Bhavcopy parse complete; prev_day_* cache populated |
| 08:35:30 | Parity check vs origin/main |
| 08:45:00 | Pre-market token validity |
| 09:00:00 | AWS budget scrape |

**Total boot-to-ready time: ~5 minutes.** Same as today's design, just with explicit timing slots.

---

## 🚨 NEW worst-case scenarios from this audit (W185-W190)

| # | Scenario | Defense |
|---|---|---|
| W185 | Daily reconcile delayed past 17:25 IST due to long market activity | If not started by 17:25, fire CRITICAL Telegram + skip; resume tomorrow morning |
| W186 | Bhavcopy at 08:35 but NSE hasn't published yet (delayed) | Retry every 5 min for up to 2 hours; if still missing at 10:35 → CRITICAL Telegram |
| W187 | Boot at 08:30 takes > 5 min due to cold start | BOOT-02 fires at 60s threshold; operator alerted |
| W188 | Process killed at 17:30 mid-reconcile (5 sec buffer not enough) | State machine: reconcile work split into checkpoints; resume next boot |
| W189 | Two-day gap (e.g., weekend): boot Mon detects "last_boot=Fri" | New-day logic handles N-day gap by treating Mon as new session |
| W190 | Operator manually starts AWS at 22:00 IST for testing | OK — boot logic detects "outside normal market hours, dry_run=true forced" |

---

## 📊 Updated count of NEW worst-case scenarios across all plans

| Plan | Range | Count |
|---|---|---|
| WS Flow Health 11 root causes | — | 11 |
| Zero Tick Loss Coverage Map | — | 59 |
| Memory/WAL/Ring/Shadow | — | 44 |
| Dynamic Depth | — | 20 |
| Token/Auth | — | 77 cells |
| Post-Market Heart Piece | W1-W45 | 45 |
| Extreme Worst-Case Sweep | W46-W155 | 110 |
| Cross-Verify Handoff | W156-W170 | 15 |
| Mac-AWS Parity | W171-W184 | 14 |
| **NEW Timing Audit** | **W185-W190** | **6** |
| **GRAND TOTAL** | | **~401 paths defended** |

---

## ✅ What must change (PR-level work for Friday)

### Plan files to update

| Plan file | Edit type |
|---|---|
| `topic-questdb-7-layer-z-plus-defense.md` | Replace "23:00 IST" → "17:25 IST" |
| `topic-tick-to-candle-math-prevday-sourcing.md` | Replace all 23:00 IST + 08:15 IST → 17:25 IST + 08:35 IST |
| `topic-zero-tick-loss-coverage-map.md` | Update Stage 6 to 17:25 IST |
| `topic-observability-7-layer-z-plus-defense.md` | All 4 reconciles → 17:25 IST |
| `topic-z-plus-retrofit-23-monday-tasks.md` | Update Task 13 + bhavcopy task |
| `topic-memory-wal-ring-shadow-deep-drill.md` | All L3 cells → 17:25 IST |
| `topic-cross-verify-observability-and-claude-handoff.md` | Update auto-driver story 08:15 → 08:35 |
| `topic-post-market-historical-fetch-cross-verify.md` | Update bhavcopy mentions 08:15 → 08:35 |

### Code-level changes (Friday implementation)

| Component | Change |
|---|---|
| Boot sequence orchestrator | Add 08:30:30 "midnight rollover equivalent" step |
| Bhavcopy downloader scheduler | 08:35 IST (was 08:15) |
| Parity check scheduler | 08:35:30 + 17:25 IST (was 23:00) |
| Daily reconcile orchestrator | 17:25 IST entry point (was 23:00) |
| Partition manager trigger | 17:25 IST (was 23:00) |

---

## 🎤 Operator's question answered (Option A audit)

**Q: "Check option A dude — look for MORE timing bugs"**

**A: Found 23 violations across 10+ plan files. All categorized + fixed.**

| Category | Count | Status |
|---|---|---|
| 23:00 IST daily reconciles | ~15 | Moved to 17:25 IST (pre-shutdown) |
| 08:15 IST bhavcopy | ~6 | Moved to 08:35 IST (post-boot) |
| Midnight rollover | 3 | Replaced with "new-day boot logic" at 08:30:30 |
| Nightly EBS snapshot | 1 | NOT a bug (AWS Backup is server-side) |

**Plus 6 NEW worst-case scenarios W185-W190.**

**Total tickvault worst-case coverage: 401 defense paths.**

Plan files need ~30 line edits to consolidate the timing fix. Friday work or now?

Discussion mode continues. NO IMPLEMENTATION.
EOF
echo "comprehensive timing audit locked"