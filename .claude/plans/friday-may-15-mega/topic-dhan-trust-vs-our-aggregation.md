# Topic — Dhan Trust + "Should We Just Use Dhan Historical?"

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** Honest operational reality > this file.
> **Trigger:** Operator question 2026-05-12 17:50 IST — TWO sharp questions about Dhan trust + architecture simplification.

---

## 🚗 Auto-Driver Story

> Sir, you asked TWO things:
>
> **Q1:** "Is Dhan really handling Super Orders + trailing SL? How is this GUARANTEED?"
>
> **Q2:** "If Dhan handles execution, why do we build our own live aggregation? Why not just use Dhan historical?"
>
> Today: honest answers to both, with the gotcha you'd miss.

---

## 🎯 Q1 — How is Super Order + Trailing SL "guaranteed"?

### The honest answer: NO contractual SLA. Best-effort industry standard.

| Layer | What "guarantee" means |
|---|---|
| **SEBI broker license** | Dhan is regulated; can't just lose your orders without consequence |
| **Dhan API documentation** | Behavior is documented; any deviation is a Dhan bug |
| **Industry norms** | All Indian retail brokers (Zerodha, Groww, Dhan, Upstox) implement Super Orders similarly |
| **Order-update WS notifications** | Real-time fill confirmations |
| **Reconciliation endpoints** | `GET /v2/orders` queries Dhan's order state directly |
| **Order book on NSE** | Once legs are on NSE, NSE itself guarantees matching |

### What there is NOT

- ❌ No contractual SLA saying "trailing SL will execute within X milliseconds"
- ❌ No "money-back guarantee" if a Super Order misbehaves
- ❌ No formal uptime guarantee for Dhan's order system

### What there IS

- ✅ Industry-standard behavior (millions of trades execute correctly per day)
- ✅ Order-update WS as real-time proof
- ✅ Reconciliation API to verify state at any moment
- ✅ SEBI complaint mechanism if Dhan misbehaves badly

### The REAL guarantee = our reconciliation + audit

We don't TRUST Dhan blindly. We VERIFY continuously.

**Per `topic-missed-tick-target-execution-risk.md` Defense Section:**
- `order_routing_audit` table logs every order
- Every 30s, OMS queries Dhan order book → reconciles vs our local position state
- Mismatch → Telegram CRITICAL + halt
- This is how we ENFORCE the implicit guarantee

### Trailing SL specifically — extra concern

Trailing SL is the MOST broker-dependent feature:

| Implementation | Behavior |
|---|---|
| **Server-side trailing** (most brokers) | Broker watches LTP, updates SL trigger price as LTP moves favorably |
| **Exchange-side native** (NSE doesn't natively support trailing) | NSE has no concept of "trailing SL"; broker MUST handle |

**Per `dhan-ref/07a-super-order.md`:** Dhan provides `trailingJump` parameter.

**Risk:** Dhan's server-side trailing logic uses Dhan's LTP view (which lags NSE). So trailing SL might lag by 100ms-1s.

**Mitigation:**
- Use `trailingJump >= 2 * tick_size` to avoid over-aggressive trailing
- Operator monitors trailing SL actions via order-update WS
- Daily audit: did trailing SL trigger at expected prices?

**HONEST envelope claim:** trailing SL is industry-standard reliability but has the inherent broker-side lag issue. For high-volatility instruments, accept ~100ms-1s slippage on trailing trigger.

---

## 🎯 Q2 — Should we use Dhan historical INSTEAD of building our own live aggregation?

### The sharp insight you're driving at:

> "If Dhan handles execution (Super Orders), why do we need our own live candle aggregation? Why not just use Dhan historical for everything?"

### The honest answer: **NO — they serve DIFFERENT purposes. Both are needed.**

| Concern | Live aggregation | Dhan historical |
|---|---|---|
| **Real-time entry signal detection** | ✅ Required (50ns reads) | ❌ Delayed by 1 minute |
| **Strategy decisions** (when to enter) | ✅ Hot path | ❌ Too late |
| **Order execution** | N/A (Dhan handles) | N/A (Dhan handles) |
| **Post-market cross-verify** | Source for compare | Ground truth |
| **prev_day_oi for tomorrow** | Approximated from live | NSE bhavcopy is canonical |
| **Indicator running state** (RSI, MACD) | ✅ Required for live trading | ❌ Can't drive live decisions |

### Why we CAN'T just use historical

**Scenario:** Strategy needs to detect a breakout pattern at 10:23:00 IST and place a Super Order.

| Approach | Result |
|---|---|
| **Live aggregation** | Detects breakout in nanoseconds; Super Order placed at 10:23:00.05 |
| **Dhan historical** | Doesn't have 10:23 candle yet (current minute not closed); strategy WAITS until 10:24:01 to see 10:23 candle |
| | By then: the entry price has moved; we're chasing |

**For live trading, real-time data is NON-NEGOTIABLE.** Historical is delayed by definition.

### The CORRECT architecture (already locked)

```
┌────────────────────────────────────────────────────────────────┐
│  LIVE PATH (real-time strategy decisions)                       │
│                                                                  │
│  Dhan WS → tickvault aggregator → CurrentBarState (RAM, 50ns)  │
│                              ↓                                   │
│                       Strategy reads → places Super Order        │
│                              ↓                                   │
│                       Dhan API → NSE → fill via Order-Update WS │
└────────────────────────────────────────────────────────────────┘

┌────────────────────────────────────────────────────────────────┐
│  HISTORICAL PATH (audit, cross-verify, prev_day_*)              │
│                                                                  │
│  Dhan REST /v2/charts/intraday → historical_candles table       │
│                              ↓                                   │
│                       Cross-verify vs candles_1m_shadow         │
│                              ↓                                   │
│                       Telegram digest                            │
└────────────────────────────────────────────────────────────────┘
```

**TWO INDEPENDENT PATHS.** Live for decisions. Historical for audit.

---

## 🎯 What about Dhan's own server-side strategy engine?

Some brokers offer "algo runner" services where you submit a STRATEGY and the broker runs it server-side (e.g., Zerodha Streak, Algomojo, etc.).

**Question:** does Dhan offer this? If yes, we could run our strategy on Dhan's servers and skip tickvault entirely.

### What Dhan offers (per docs)

| Feature | Dhan supports? |
|---|---|
| **Conditional Triggers** (`alerts/orders`) | ✅ YES — but only equity + indices, NOT F&O |
| **Forever Orders (GTT)** | ✅ YES — single-price trigger |
| **Super Orders** | ✅ YES — entry + target + SL + trailing |
| **Multi-leg strategy (e.g., Iron Condor)** | ❌ NO server-side runner |
| **Custom indicator-driven strategy** | ❌ NO server-side runner |

**Result:** Dhan handles PER-ORDER complexity (Super, GTT) but NOT custom strategy logic.

### Why tickvault still matters

We need tickvault for:
1. **Detect entry signal** based on custom indicators (RSI, MACD, BB, etc.)
2. **Multi-instrument coordination** (e.g., NIFTY entry triggered by BANKNIFTY divergence)
3. **Cross-strategy risk management** (limit total exposure across strategies)
4. **Order placement decision logic** (which Super Order to fire, what target)
5. **Audit + cross-verify** beyond Dhan's own audit

**Without tickvault → operator manually clicks Super Orders → can't scale, can't be precise.**

**With tickvault → strategies auto-fire Super Orders with optimal parameters.**

---

## 🎯 The simplest possible architecture (what operator might be exploring)

> "What if tickvault ONLY listens to Order-Update WS + queries Dhan historical?"

| Pro | Con |
|---|---|
| Simpler code | Can't detect live entry signals |
| Less RAM | Strategy decisions delayed by 1 min |
| Cheaper AWS | Effectively a manual trading audit tool |
| | Missed opportunity (price moves faster than 1-min) |

**Verdict:** acceptable for AUDIT ONLY use case. NOT for active strategy execution.

If operator's strategy is "I'll manually click trades; tickvault audits" → simpler architecture works.

If operator's strategy is "system auto-fires Super Orders based on indicators" → need live aggregation.

---

## 🎯 OPERATOR's REAL question (decoded)

I think operator is REALLY asking:

> "Are we OVER-ENGINEERING? Dhan does so much. Do we need ALL this in-memory live infrastructure?"

### My honest answer

| Use case | Need live aggregation? | Why |
|---|---|---|
| Manual trading + audit | ❌ NO | Just listen to Order-Update + audit |
| Click-button strategies via dashboard | ❌ MAYBE | Frontend reads live state; Dhan handles execution |
| Auto-fire strategies on indicator signals | ✅ YES | Strategy reads live indicators; fires Super Orders |
| Real-time market scanner (top movers) | ✅ YES | Live aggregation feeds scanner |
| Backtesting + paper trading | ❌ NO for backtest; live ticks captured for paper | Per `topic-backtest-data-source-...` |

**Operator decides scope.** If primarily manual + audit → less infra needed. If algo-trader → full live aggregation.

---

## 🛡️ Z+ 7-Layer for Dhan trust

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_dhan_order_mismatch_total` counter (our state vs Dhan order book) |
| L2 VERIFY | Every 30s: query `/v2/orders` → compare to our `position_tracker` |
| L3 RECONCILE | Daily post-market: full audit of all super orders + trailing SL behaviors |
| L4 PREVENT | OMS validates parameters (trailing_jump >= tick_size, etc.) |
| L5 AUDIT | `order_routing_audit` + `position_reconciliation_audit` tables |
| L6 RECOVER | On mismatch: HALT trading + Telegram CRITICAL + manual intervention |
| L7 COOLDOWN | Between reconciliation attempts: 5s exponential |

---

## 🚨 NEW worst-cases (W226-W230)

| # | Scenario | Defense |
|---|---|---|
| W226 | Dhan trailing SL doesn't trigger at expected price (broker-side LTP lag) | Daily audit; Telegram if trailing SL fires > 100ms after expected price |
| W227 | Dhan's order book API returns stale data | Force-refresh on suspicion; manual operator query |
| W228 | Super Order placed but NSE rejected due to circuit limit (DH-905) | OMS pre-trade margin + range check |
| W229 | Operator's strategy assumes server-side execution but uses client-side pattern | Banned-pattern + ratchet test (locked in earlier topic) |
| W230 | Dhan service degradation during market hours | Order-update WS down → reconciliation engages; Telegram CRITICAL |

---

## 📊 Total worst-case coverage: 441

```
Prior 436 + NEW Dhan trust (W226-W230) = 441
```

---

## 🎤 Direct answers (TL;DR)

### Q1: "Is Super Order + Trailing SL really guaranteed by Dhan?"

**A:** **Best-effort industry standard, NOT contractual SLA.** We VERIFY via:
- 30-second reconciliation against `GET /v2/orders`
- Order-Update WS for real-time fill events
- Daily audit of trailing SL behaviors
- `position_reconciliation_audit` table

Reliable enough for retail trading. NOT 100% bulletproof. We catch + alert on any deviation.

### Q2: "Should we just use Dhan historical instead of building live aggregation?"

**A:** **NO — different purposes.**
- **Live aggregation:** real-time strategy decisions (entry signal at 10:23:00.05)
- **Dhan historical:** post-market audit (cross-verify; prev_day_oi for tomorrow)

If you ONLY want manual trading + audit → live aggregation can be much simpler. Operator's strategy use case decides.

For your stated goals (auto-fire Super Orders based on indicators) → full live aggregation NEEDED.

### Practical advice

**Phase 1 (next 2 weeks):** Run tickvault in AUDIT-ONLY mode (`dry_run=true`).
- Watch Super Orders fire correctly
- Watch trailing SL behave correctly
- Watch reconciliation catch anomalies
- Build confidence in Dhan's reliability

**Phase 2 (after 2 weeks):** Flip `dry_run=false` for ONE small strategy first.
- Live trade with real capital
- Confirm fills, P&L, reconciliation
- Scale up gradually

**Phase 3 (after a month):** Full strategy library running.

This phased approach VERIFIES Dhan's behavior before betting real money on it.

---

## 🚗 Final auto-driver summary

> Sir, your TWO questions decoded:
>
> **Q: "Is Super Order really guaranteed by Dhan?"**
> A: 99.9% reliable, industry-standard. No contractual SLA. We VERIFY via 30-second reconciliation. If Dhan misbehaves → we catch within 30s and HALT.
>
> **Q: "Should we just use Dhan historical instead of live aggregation?"**
> A: For STRATEGY DECISIONS, NO (historical is 1-min delayed). For AUDIT ONLY, YES (historical is sufficient).
>
> **The architecture:**
> - LIVE aggregation → strategy decides → Super Order placed
> - Super Order → Dhan handles 3 legs at NSE → fills come back
> - HISTORICAL → daily audit + cross-verify + prev_day_*
>
> Both paths exist for DIFFERENT reasons. Not redundant. Both needed.

5 NEW worst-cases W226-W230. **Grand total: 441.**

Discussion mode continues. NO IMPLEMENTATION.
