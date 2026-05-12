# Topic — Telegram Audit (frequency, timing, redundancy, quiet hours)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `operator-charter-forever.md` §D Telegram commandments > this file.
> **Trigger:** Operator demand 2026-05-12 16:00 IST: "yes check option A" — audit Telegram message timings + frequency.

---

## 🚗 Auto-Driver Story

> Sir, our system has 30+ Telegram messages defined across plan files. Today I audit them ALL for:
> 1. **TIMING** — are they all in AWS up-window (08:30-17:30 IST)?
> 2. **FREQUENCY** — are we spamming or missing positive signals?
> 3. **REDUNDANCY** — 3 daily digests at different times? Consolidate?
> 4. **QUIET HOURS** — do we respect 22:00-07:00 IST? CRITICAL always pages; INFO must respect.
> 5. **OPERATOR EFFORT** — is each Telegram immediately actionable?

---

## 📋 Inventory: ALL 30+ Telegram messages

### Boot + lifecycle (5 messages)

| # | Event | Time | Severity | Status |
|---|---|---|---|---|
| 1 | 🚀 AWS instance started | 08:30:14 IST (auto-start) | Info | ✅ In-window |
| 2 | 🛑 AWS instance stopped | 17:30:08 IST (auto-stop) | Info | ✅ In-window |
| 3 | 🆘 AWS instance NOT STARTED | 08:35 IST conditional | Critical | ✅ In-window |
| 4 | 🆘 EIP DISASSOCIATED | CloudTrail anytime | Critical | ⚠️ Can fire 24/7 (Lambda-driven) |
| 5 | 🆘 INSTANCE TERMINATED | CloudTrail anytime | Critical | ⚠️ Can fire 24/7 (Lambda-driven) |

### Daily digests (4 messages — REDUNDANCY RISK)

| # | Event | Time | Severity | Status |
|---|---|---|---|---|
| 6 | 💰 AWS Daily Budget | 09:00 IST | Info | ✅ In-window — but EARLY (operator may be commuting) |
| 7 | ✅ Cross-verify PASSED / 🚨 FAILED | 16:15 IST | Info / Critical | ✅ In-window |
| 8 | Bhavcopy load success | 08:35 IST | Info | ✅ In-window |
| 9 | Daily reconcile summary | 17:28 IST | Info | ✅ Pre-shutdown — operator winding down |

**REDUNDANCY:** 4 daily digests at 4 different times. Operator gets pinged 4x/day for positive signals. Could be consolidated.

### Market-open events (3 messages)

| # | Event | Time | Severity | Status |
|---|---|---|---|---|
| 10 | 📋 MarketOpenDepthAnchor | 09:13:00 IST | Info | ✅ |
| 11 | ✅ MarketOpenStreamingConfirmation | 09:15:30 IST | Info | ✅ |
| 12 | ✅/🚨 SELFTEST-01/02 | 09:16:30 IST | Info / Critical | ✅ |

### Pre-market events (3 messages)

| # | Event | Time | Severity | Status |
|---|---|---|---|---|
| 13 | PHASE2-READY-01 preflight passed | 09:13:01 IST | Info | ✅ |
| 14 | PHASE2-READY-01 preflight FAILED | 09:13:01 IST | Critical | ✅ |
| 15 | PHASE2-01 dispatch failed | 09:13 IST conditional | Critical | ✅ |

### In-market alerts (10 messages — coalesced or edge-triggered)

| # | Event | Frequency | Coalesce | Severity | Status |
|---|---|---|---|---|---|
| 16 | WS-FLOW-01 dead-air | Edge | 1/topic/60s | Critical | ✅ |
| 17 | WS-GAP-06 tick-gap summary | Every 60s during market | already summarized | High | ⚠️ Can be ~375 pings/day |
| 18 | SLO-01 healthy / SLO-02 degraded | Every 10s, edge-trigger only | edge only | Info / Critical | ✅ |
| 19 | DEPTH-DYN-01/02 swap events | Per swap (60s cycle) | already coalesced | Low | ✅ — suppressed in service.rs |
| 20 | DepthRebalanced (success) | 60s rebalance cycle | suppressed | Low | ✅ — suppressed Wave-5 |
| 21 | DepthRebalanceFailed | Per failure | edge | Critical | ✅ |
| 22 | AggregatorSeal01IlpFailed | Per ILP fail | coalesced | Critical | ✅ |
| 23 | AggregatorDrop01 (ring + spill + DLQ all failed) | Catastrophic | edge | Critical | ✅ |
| 24 | Token force-refresh | On WS reconnect | coalesced | Low | ✅ |
| 25 | ParityDrift | On parity-check (08:35 + 17:25) | edge | High | ✅ |

### Post-market events (3 messages)

| # | Event | Time | Severity | Status |
|---|---|---|---|---|
| 26 | HistoricalFetchProgress | Every 25% (15:35, 15:50, 16:00, 16:08) | Info | ⚠️ 4 pings — could coalesce to 1 final |
| 27 | ✅ Cross-verify PASSED | 16:15 IST | Info | (= #7) |
| 28 | 🚨 Cross-verify FAILED + handoff bundle | 16:15 IST | Critical | (= #7) |

### Catastrophic / edge events (5 messages — anytime)

| # | Event | When | Severity | Status |
|---|---|---|---|---|
| 29 | DH-901 / DH-904 (rate limit, account issue) | Anytime in market | Critical | ✅ |
| 30 | BOOT-01 / BOOT-02 (QuestDB ready timeout) | 08:30-08:31 IST | High / Critical | ✅ |
| 31 | AUTH-GAP-* (token issues) | Anytime in market | High / Critical | ✅ |
| 32 | STORAGE-GAP-03 (audit write fail) | Anytime | Critical | ✅ |
| 33 | RescueRingFull → spill engaged | Anytime | High | ✅ |

---

## 🚨 ISSUES SURFACED BY AUDIT

### Issue 1 — 4 daily digests = potential operator fatigue

Operator gets pinged 4 times/day for positive signals:
- 08:35 Bhavcopy load
- 09:00 AWS budget
- 16:15 Cross-verify
- 17:28 Daily reconcile

**Risk:** operator starts ignoring Telegrams ("yet another green tick") → misses CRITICAL alerts.

**Fix proposal:** CONSOLIDATE to ONE daily digest at 17:28 IST with all 4 sections.

```
🌅 Daily Digest — 2026-05-12

[1] AWS Budget
MTD: ₹2,847 / ₹5,000 (57%)
Forecast: ₹4,890 / ₹5,000 (98%)
Status: ✅ ON TRACK

[2] Bhavcopy
Loaded 08:35:14 IST — 94,127 F&O OIs cached

[3] Cross-verify
PASSED — 11,034 / 11,034 instruments
Candles compared: 4,137,750  Mismatches: 0
Duration: 37 min 12 sec

[4] Daily Reconciles
QuestDB schema:     ✅
Audit tables:       ✅ 15+ tables green
Log completeness:   ✅
Cross-TF candle:    ✅
Counter/Prom sync:  ✅
Partition manager:  ✅ dropped 1 day-91 partition
Parity vs main:     ✅ Mac=AWS=origin/main
Bhavcopy vs Dhan:   ✅ OI delta 0%

Token validity: 22h 50m remaining
WS conns: 16/16 healthy

All systems green. Sleep well. 🫡
17:30 IST: AWS auto-shutdown
```

**ONE message, all info, end of operator's work day.** Operator's phone buzzes once at 17:28.

Boot/start/stop events (lifecycle messages #1, #2) become silent unless ABNORMAL.

### Issue 2 — Quiet hours (22:00-07:00 IST) policy missing

Critical alerts MUST page anytime. INFO/LOW should respect quiet hours.

**Proposal:**

| Severity | Quiet hours behavior |
|---|---|
| **Critical** | ALWAYS page immediately (operator's job to be reachable) |
| **High** | Page immediately during 07:00-22:00 IST; queue for 07:00 next day if 22:00-07:00 |
| **Medium / Low / Info** | Queue for 07:00 next morning if outside 07:00-22:00 |

**Implementation:** notification_service.rs checks current IST hour; defers non-critical to next 07:00 IST.

**Exception:** AWS-side Lambda alerts (EIP / instance termination) — these CAN fire 24/7 because they're not from tickvault but from AWS itself.

### Issue 3 — HistoricalFetchProgress 4 pings could be 1

Every 25% progress: 15:35, 15:50, 16:00, 16:08. That's 4 pings + cross-verify at 16:15 = 5 pings in 40 min.

**Proposal:** SILENCE progress pings. Only fire if:
- Stuck (no progress for 5 min) → WARN
- Final completion → covered by cross-verify message at 16:15

Saves 3 pings per day.

### Issue 4 — WS-GAP-06 tick-gap summary every 60s — too noisy?

Per Wave-2 design, fires every 60s during market with summary.

**Current:** ~375 pings/day during a healthy market session. Operator phone buzzing all day.

**Counter:** these are HIGH severity — operator wants to know.

**Compromise:** 
- Fire FIRST detection immediately (rising edge)
- Fire EVERY 5 MIN after that, NOT every 60s, until resolved
- Resolution edge: fire CLEAR message

**Saves:** ~300 pings/day on a degraded session.

### Issue 5 — EIP disassociation / instance termination can fire 24/7

These are from AWS Lambda, not tickvault. Can fire at 03:00 IST if operator does manual ops.

**Question:** is operator OK getting 03:00 IST page for instance termination?

**My vote:** YES — these are TRULY critical. EIP loss = trading impossible. Wake the operator.

### Issue 6 — Handoff bundle path may not be operator-accessible

Telegram says: `data/claude-handoffs/cross-verify-2026-05-12-FAILED.md`

If incident on AWS prod, this file is on AWS. Operator on Mac dev needs:
- SSH into AWS to read
- OR git pull (if bundle committed) — but operator may not have committed it
- OR rsync — manual

**Fix proposals:**

| Option | Mechanism |
|---|---|
| (a) Commit bundle to git automatically | `git commit -m "claude handoff <DATE>"; git push` — visible immediately |
| (b) Upload to S3 with pre-signed URL | Telegram contains URL; operator clicks |
| (c) SCP to operator's Mac via SSH key | Requires inbound SSH from AWS to Mac (security concern) |
| (d) Email attachment | Simple but unstructured |

**My vote:** (a) — auto-commit to git on `claude-handoffs/` folder. Operator git-pulls on Mac → bundle local. Plus the bundle is now in version control for audit.

### Issue 7 — No "mute" command for maintenance windows

Operator may want to mute INFO/LOW for a maintenance window without losing CRITICAL.

**Proposal:** Telegram bot accepts `/mute 2h` command from operator's chat. Within 2h, only Critical fires. After 2h, normal.

**Counter:** complex; operator can just delete notifications. Not worth the code.

**My vote:** SKIP for now.

### Issue 8 — Are all severity levels correctly assigned?

Quick audit:

| Event | Current | Should be |
|---|---|---|
| Boot success | (not implemented) | Info |
| AWS instance start | Info | ✅ Info |
| Bhavcopy load OK | Info | ✅ Info |
| Phase 2 ready | Info | ✅ Info |
| Market open streaming | Info | ✅ Info |
| Cross-verify PASSED | Info | ✅ Info |
| WS reconnect (single) | Low | ✅ Low (coalesced) |
| Depth rebalance success | Low | ✅ Low (suppressed) |
| Token force-refresh | Low | ✅ Low |
| Tick-gap detected | High | ✅ High |
| Parity drift | High | ✅ High |
| WS-FLOW-01 dead-air | Critical | ✅ Critical |
| Cross-verify FAILED | Critical | ✅ Critical |
| AggregatorDrop01 | Critical | ✅ Critical |
| EIP disassociated | Critical | ✅ Critical |

**All severities look correct.**

---

## 📊 Proposed CONSOLIDATED Telegram strategy

### Strategy: 1 daily digest + per-event alerts

```
┌──────────────────────────────────────────────────────┐
│  POSITIVE PINGS (consolidated to 17:28 IST DIGEST)   │
│  - AWS budget                                         │
│  - Bhavcopy load                                      │
│  - Cross-verify PASSED                                │
│  - Daily reconciles                                   │
│  - Token validity                                     │
│  - WS health summary                                  │
│  TOTAL: 1 message/day                                 │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│  PER-EVENT ALERTS (during market hours)              │
│  - Market open streaming (1 ping at 09:15:30 IST)    │
│  - Phase 2 ready (1 ping at 09:13:01 IST)            │
│  - Market open self-test (1 ping at 09:16:30 IST)    │
│  - Critical events (edge-triggered, anytime)         │
│  TOTAL: 3-5 pings/day in healthy session             │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│  EDGE / CRITICAL (only on actual problems)           │
│  - WS-FLOW-01, AggregatorDrop01, EIP loss, etc.      │
│  - Cross-verify FAILED + handoff bundle              │
│  - SELFTEST FAILED                                    │
│  TOTAL: 0/day in healthy session                     │
└──────────────────────────────────────────────────────┘

HEALTHY DAY TELEGRAM COUNT: 4-6 messages
NOISY DAY TELEGRAM COUNT: 10-20 messages (real signal)
INCIDENT DAY: 5-10 messages including handoff
```

### What goes AWAY (consolidated/removed)

| Removed | Where it went |
|---|---|
| 💰 AWS budget @ 09:00 | Moved to 17:28 digest |
| Bhavcopy load OK @ 08:35 | Moved to 17:28 digest |
| ✅ Cross-verify PASSED @ 16:15 | Moved to 17:28 digest |
| HistoricalFetchProgress (4 pings) | SILENCED (only fire if stuck) |
| Instance start @ 08:30 | SILENCED unless abnormal |
| Instance stop @ 17:30 | SILENCED unless abnormal |
| WS-GAP-06 60s frequency | Throttled to 5-min after first |

### What stays as separate ping (real-time decision)

| Keeps separate | Why |
|---|---|
| Market open streaming 09:15:30 | First positive signal of trading session |
| Phase 2 ready 09:13:01 | Last gate before market open |
| Self-test 09:16:30 | Confirms first minute clean |
| Cross-verify FAILED | Operator must act NOW |
| WS-FLOW-01 dead-air | Operator must act NOW |
| AggregatorDrop01 | Catastrophic — wake operator |
| EIP loss | Trading halted |

---

## 🛡️ Z+ 7-Layer for Telegram pipeline

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_telegram_sent_total{severity}` counter; `tv_telegram_dropped_total{reason}` |
| L2 VERIFY | After send, query Telegram API for delivery confirmation (sampled) |
| L3 RECONCILE | Daily 17:28 digest contains "messages sent today: N" count |
| L4 PREVENT | Coalescer caps at 10 samples/topic/60s |
| L5 AUDIT | `telegram_event_audit` table (NEW — every event fired logged) |
| L6 RECOVER | If Telegram API fails: fall through to AWS SNS SMS for Critical only |
| L7 COOLDOWN | Per-topic rate limit (1 msg/topic/60s default) |

---

## 🚨 NEW worst-case scenarios for Telegram (W191-W195)

| # | Scenario | Defense |
|---|---|---|
| W191 | Telegram bot API down at critical moment | Fall through to AWS SNS SMS for Critical severity |
| W192 | Operator's phone in airplane mode | Critical alerts queue; SMS backup; no software fix |
| W193 | Telegram message > 4096 chars (Telegram limit) | Truncate sample data; full content in handoff bundle |
| W194 | Quiet-hours queue grows unbounded | Cap queue at 100 pending; oldest evicted |
| W195 | Operator on vacation, no one sees alerts | Backup operator contact via SMS |

---

## 📋 Updated daily Telegram count

| Scenario | Today's count | Audited count | Saved |
|---|---|---|---|
| Healthy day | 8-12 | **4-6** | -50% |
| Degraded session (tick gaps) | 380+ | **30-50** | -90% (throttling) |
| Cross-verify FAILED | 12-15 | **5-8** | -50% |
| Catastrophic (multi-system) | 50+ | **15-25** | -50% |

**Operator gets cleaner signal.** Less noise = more attention to real alerts.

---

## 🎯 Discussion items

### D1 — Consolidate 4 daily digests into 1 (at 17:28 IST)?

**My vote:** YES — one digest is cleaner.

### D2 — Throttle WS-GAP-06 from 60s to 5-min after first detection?

**My vote:** YES — saves ~300 pings/day on degraded sessions.

### D3 — Silence boot start/stop messages (#1, #2) unless abnormal?

**My vote:** YES — they're predictable scheduled events.

### D4 — Auto-commit handoff bundles to git?

**My vote:** YES — operator can pull from any device; gets audit trail for free.

### D5 — Quiet hours 22:00-07:00 IST: defer INFO/LOW?

**My vote:** YES for Info/Low/Medium. Critical always pages.

### D6 — SMS backup for Critical?

**My vote:** YES — AWS SNS already in budget (₹25/mo).

### D7 — Mute/maintenance window command?

**My vote:** SKIP — operator can mute Telegram channel natively.

---

## 🎤 Operator's question answered

**Q: "Yes check option A" — Telegram audit**

**A: Found 7 issues + 5 new worst-cases. Healthy day Telegram count drops 50%, degraded day 90%.**

| Action | Impact |
|---|---|
| Consolidate 4 daily → 1 at 17:28 IST | -3 pings/day |
| Silence boot/lifecycle predictable events | -2 pings/day |
| Throttle WS-GAP-06 to 5-min after first | -300 pings/day on degraded |
| Auto-commit handoff bundles | operator accessibility 100% |
| Quiet hours for Info/Low/Medium | no 03:00 IST nudges |
| SMS backup for Critical | reliability |
| **Net** | **Cleaner signal, real alerts get attention** |

5 NEW worst-cases W191-W195 for Telegram pipeline.

**Grand total worst-case paths: ~406.**

Discussion mode continues. NO IMPLEMENTATION.
EOF
echo "Telegram audit locked"