# Topic — 1-Month AWS Live Monitoring Phase (the path forward)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** Operator decision 2026-05-13 03:30 IST > this file.
> **Trigger:** Operator: "Only choice is to deploy live trading WS in AWS, monitor for 1 month minimum, only then will we KNOW real drift + WS disconnect patterns."

---

## 🚗 Auto-Driver Story

> Sir, you're EXACTLY right. We've debated 30+ topics about Dhan vs NSE drift, WS disconnects, sampling, missed ticks. **The truth is: we can ONLY KNOW by running it live for 1 month and measuring.**
>
> Phase 1 (1 month, dry_run=true): collect REAL data on:
> - How often does WS actually disconnect?
> - How big is real drift live vs historical?
> - Does Super Order behave as documented?
> - Does trailing SL trigger correctly?
>
> After 1 month, we have FACTS instead of theory. We decide Phase 2 (live trading) with confidence.

---

## 🎯 Why 1 month is the right monitoring window

| Period | Statistical confidence |
|---|---|
| 1 day | Single data point — meaningless |
| 1 week | 5 trading days — directional only |
| **1 month (~22 trading days)** | **Sufficient sample for typical patterns** |
| 3 months | Better, but takes longer to learn |
| 1 year | Optimal but too slow for go-live decision |

**1 month covers:**
- Multiple Mondays / Fridays (different liquidity)
- 1-2 expiry weeks (volatility extremes)
- 1 month-end settlement
- Various market conditions (trending, ranging, news)
- Multiple post-market cross-verify cycles
- Multiple AWS auto-start/stop cycles
- Multiple WS reconnection events

**1 month = enough to spot patterns. Not so long that go-live is delayed.**

---

## 📊 What to MEASURE during the 1-month phase

### Category A — WS health metrics

| Metric | Target | Source |
|---|---|---|
| WS disconnect events per day | < 5 | `tv_websocket_disconnect_total` |
| Average disconnect duration | < 30s | `tv_websocket_disconnect_duration_seconds` |
| Reconnect success rate | > 99% | `tv_websocket_reconnect_success_total` |
| Subscriptions preserved across reconnect | 100% | SubscribeRxGuard ratchet |
| WS dead-air events (Connected but no ticks) | < 1/day | `tv_ws_dead_air_total` |
| Tick gap > 30s (per instrument) | < 10/day per instrument | WS-GAP-06 |
| Soft restart escalation count | 0 | `tv_ws_soft_restart_total` |

### Category B — Data quality metrics

| Metric | Target | Source |
|---|---|---|
| Live OHLC vs REST historical drift (per minute) | < tick size (₹0.05) | Daily cross-verify summary |
| Cross-verify mismatch rate (per day) | 0 (zero tolerance) | `cross_verify_mismatches` table |
| Volume drift % | < 0.5% | Per-day audit |
| OI drift % (vs bhavcopy) | < 0.1% | Daily reconcile |
| prev_day_close availability | 100% by 09:00 IST | bhavcopy load audit |
| prev_day_oi coverage | > 99% F&O instruments | bhavcopy parse audit |

### Category C — Order execution metrics (dry_run=true)

| Metric | Target | Source |
|---|---|---|
| Super Order placement success rate | > 99.9% | `order_routing_audit` |
| Super Order entry → target/SL placement latency | < 500ms | OMS timing |
| Trailing SL update frequency | per Dhan behavior | Order-Update WS |
| Order-Update WS missed events | 0 | Reconciliation diff |
| Position state consistency (us vs Dhan) | 100% | 30-sec reconciliation |
| Phantom fill events (Dhan reports fill we didn't expect) | 0 | Audit |
| Missed fill events (Dhan filled, we didn't know) | 0 | Reconciliation catches |

### Category D — Strategy metrics (dry_run=true)

| Metric | Target | Source |
|---|---|---|
| Strategy entry signals fired per day | strategy-dependent | Strategy log |
| Entry → Super Order placement latency | < 100ms | Strategy → OMS audit |
| Hypothetical P&L (if dry_run was off) | calculated | Position tracker |
| Backtest vs paper trade drift | < 1% per day | Comparison |
| Slippage (entry hypothetical vs realistic) | < 0.1% on liquid | Calculated |

### Category E — System health metrics

| Metric | Target | Source |
|---|---|---|
| AWS instance uptime (within 08:30-17:30 window) | 100% | EventBridge + watchdog |
| AWS budget MTD vs ₹5K cap | < 100% | Cost Explorer |
| QuestDB disk usage | < 80% | df gauge |
| RAM usage (app) | < 1.95 GB | RSS gauge |
| Bhavcopy load success | 100% trading days | Daily audit |
| Cross-verify completion | 100% trading days | Daily audit |
| Telegram delivery success | > 99% | Coalescer audit |

---

## 🎯 The 1-month monitoring schedule

### Week 1 (Days 1-5)
- **Goal:** infrastructure stability
- **Watch:** AWS auto-start/stop, Docker health, WS first-time connection
- **Interventions:** fix any boot-sequence issues
- **Decisions:** continue if no critical bugs

### Week 2 (Days 6-10)
- **Goal:** strategy signal accuracy
- **Watch:** signal generation rate, false positive rate, indicator drift
- **Interventions:** tune indicator parameters if needed
- **Decisions:** strategy logic confirmed

### Week 3 (Days 11-15) — INCLUDES BANKNIFTY EXPIRY
- **Goal:** stress-test under high-volatility expiry
- **Watch:** WS health under load, mass-RST patterns, depth feed reliability
- **Interventions:** none (observe + record)
- **Decisions:** validate plan for expiry-day trading

### Week 4 (Days 16-22)
- **Goal:** end-of-month settlement + cross-month transition
- **Watch:** monthly contract expiry, F&O ban list updates, settlement-day OI changes
- **Interventions:** ensure F&O rollover handled
- **Decisions:** monthly transition validated

### Day 23-30
- **Goal:** statistical analysis + go/no-go decision
- **Watch:** aggregate metrics across the month
- **Interventions:** none (analysis only)
- **Decisions:** Phase 2 ready or extend Phase 1

---

## 📋 Decision criteria after 1 month

### Green-light Phase 2 (flip dry_run=false on ONE strategy) IF:

| Criterion | Threshold |
|---|---|
| Cross-verify mismatch days | 0 of 22 |
| WS dead-air events | < 1 per day average |
| Soft restart escalations | 0 in month |
| Bhavcopy load failures | < 5 days of 22 (with fallback working) |
| Super Order placement success | > 99.5% |
| Position reconciliation mismatches | 0 |
| AWS budget | < ₹5K |
| RAM usage trend | stable (no leaks) |

### Yellow-light (extend Phase 1 by 2 more weeks) IF:

| Criterion | Threshold |
|---|---|
| Any 1-2 above thresholds breached | by < 50% |
| WS dead-air events | 1-3 per day |
| Cross-verify mismatch days | 1-3 of 22 |

### Red-light (HALT, redesign needed) IF:

| Criterion | Threshold |
|---|---|
| Soft restart escalations > 0 | indicates broken design |
| Cross-verify mismatch > 3 days | data quality issue |
| Super Order failure > 1% | OMS broken |
| Position reconciliation mismatches > 0 | dangerous |
| AWS budget > 110% | overspend |

---

## 🛡️ Z+ 7-Layer for the 1-month phase

| Layer | Mechanism |
|---|---|
| L1 DETECT | Daily Telegram digest covering all metrics |
| L2 VERIFY | Weekly grep of `errors.jsonl` for novel signatures |
| L3 RECONCILE | Weekly review of all metrics vs thresholds |
| L4 PREVENT | dry_run=true ensures NO real money risk during phase |
| L5 AUDIT | All metrics persisted in QuestDB audit tables |
| L6 RECOVER | If critical metric breach: HALT + investigate before continuing |
| L7 COOLDOWN | Phase 2 only after 22+ trading days successful |

---

## 📊 NEW Telegram weekly digest (during Phase 1)

```
📊 Phase 1 Week 2 Digest — 2026-05-19 to 2026-05-23

WS Health
  Disconnect events: 12 (avg 2.4/day)
  Avg duration: 18s
  Reconnect success: 100%
  Dead-air events: 0
  Soft restarts: 0

Data Quality
  Cross-verify mismatches: 0 of 5 days
  Live vs REST drift: avg 0.04% (within tolerance)
  Bhavcopy success: 5/5 days
  prev_day_oi coverage: 99.7%

Order Execution (dry_run=true)
  Super Order placements: 47
  Successful: 47 (100%)
  Avg entry→legs latency: 215ms
  Trailing SL updates: 18
  Position reconciliation mismatches: 0

Strategy
  Entry signals fired: 47
  Hypothetical P&L: +₹4,287 (if dry_run was off)
  Backtest vs paper drift: 0.7%

System
  AWS uptime: 100%
  AWS budget MTD: ₹2,341 (₹5,000 cap, 47%)
  RAM: peak 1.78 GB (cap 2.0 GB, 89%)
  QuestDB disk: 12 GB used of 100 GB

Status: ✅ ON TRACK for Phase 2 transition (Week 5)
```

---

## 🚨 NEW worst-cases for Phase 1 monitoring (W256-W260)

| # | Scenario | Defense |
|---|---|---|
| W256 | Critical metric breached but operator doesn't notice | Daily Telegram + weekly digest |
| W257 | Dry_run=true accidentally flipped to false | OMS safeguard: explicit confirm prompt during boot |
| W258 | Strategy fires signal that would be a real loss in live mode | Hypothetical P&L calculated; operator reviews |
| W259 | Bug discovered Day 21 → must extend Phase 1 | Clear yellow-light criteria; no rush to Phase 2 |
| W260 | Phase 1 successful but operator hesitates on Phase 2 | OK — extend Phase 1 indefinitely; better safe than sorry |

---

## 📊 Total worst-case coverage: 471

```
Prior 466 + NEW Phase 1 monitoring (W256-W260) = 471
```

---

## 🎯 Phased rollout (FINAL — operator-decision aligned)

```
┌─────────────────────────────────────────────────────────────┐
│  PHASE 0 — Friday build (1 day)                              │
│  Build all Wave A/B/C/D items per locked plans               │
│  Deploy to AWS; smoke test; confirm boot sequence            │
│  Status: dry_run=true forced                                 │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  PHASE 1 — 1-Month MONITORING (22 trading days)              │
│  AWS production runs daily 08:30-17:30 IST                   │
│  All data captured; daily Telegram digest + weekly summary   │
│  Cross-verify, reconciliation, audit ALL active             │
│  Strategies fire signals (dry_run=true) + record P&L         │
│  Operator REVIEWS daily/weekly; no real money at risk        │
└─────────────────────────────────────────────────────────────┘
                              ↓
                      Decision criteria check
                       (green/yellow/red)
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  PHASE 2 — ONE small strategy LIVE (2 weeks)                 │
│  Flip dry_run=false on ONE strategy with smallest capital    │
│  Watch fills, P&L, slippage closely                          │
│  Continue monitoring all Phase 1 metrics                     │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  PHASE 3 — FULL strategy library (after 1 more week)         │
│  Enable all finalized strategies                             │
│  Continuous monitoring + adaptation                          │
│  Operational steady-state                                    │
└─────────────────────────────────────────────────────────────┘
```

**Total time from Friday build to fully-live: ~6-7 weeks.**

---

## 🎤 Direct answer to operator

### Q: "Only choice is to deploy live WS in AWS, monitor for 1 month minimum to know real drift + WS disconnect issues — am I right?"

### A: **YES — 100% RIGHT.**

The 30+ topic discussions we've had this week were valuable for DESIGN. But the only way to TRUTH-TEST the design is **1 month of LIVE production data**.

**1-month phase 1 will tell us:**
1. ✅ How often WS disconnects in production (theory: rare; reality: TBD)
2. ✅ Real drift between live WS and REST historical (theory: < tick size; reality: TBD)
3. ✅ Super Order behavior under various market conditions (theory: NSE-real; reality: TBD)
4. ✅ Trailing SL accuracy (theory: 100ms-1s lag; reality: TBD)
5. ✅ Bhavcopy reliability (theory: 99%; reality: TBD)
6. ✅ Cross-verify mismatch rate (theory: 0; reality: TBD)
7. ✅ AWS budget actual vs forecast (theory: ₹4,981; reality: TBD)
8. ✅ Strategy P&L behavior in real conditions (theory: backtest = live; reality: TBD)

**After 1 month:** decide Phase 2 with FACTS, not theory.

This is the SCIENTIFIC METHOD applied to trading systems. Theorize → measure → adjust → measure again.

---

## 🚗 Final auto-driver summary

> Sir, you nailed it.
>
> All our debates about "is Dhan accurate?", "will WS disconnect?", "will trailing SL work?", "will bhavcopy be reliable?" — these are HYPOTHESES.
>
> The ONLY way to KNOW: deploy to AWS, run for 1 month with dry_run=true, MEASURE everything.
>
> After 1 month:
> - We have REAL drift numbers (not theory)
> - We have REAL disconnect frequency (not estimates)
> - We have REAL Super Order behavior (not docs)
> - We have REAL strategy P&L (not backtest)
>
> Then Phase 2 (one strategy live) → Phase 3 (full library) → steady-state.
>
> Total: ~6-7 weeks from Friday build to fully-live.
>
> **No more theory. Time to measure reality.**

5 NEW worst-cases W256-W260. **Grand total: 471.**

3-day brainstorm window closes on 2026-05-14. Friday 2026-05-15 = build day. Discussion mode continues until then.
