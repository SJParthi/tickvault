# Topic — Backtest Data Source: Live Ticks vs Dhan Historical

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** Honest data engineering reality > this file.
> **Trigger:** Operator question 2026-05-12 17:00 IST: "for backtesting, is it better to use live ticks (Groww + Dhan) or historical data from Dhan?"

---

## 🚗 Auto-Driver Story

> Sir, imagine you want to test a NEW recipe for orange juice. You have 2 sources of oranges:
> 1. **Live ticks** — oranges that arrived at YOUR shop while you were open (some boxes lost in transit)
> 2. **Dhan historical** — the FULL DAY'S delivery from Dhan's warehouse (complete, reconciled)
>
> For TESTING the recipe, you want the FULL day's oranges (complete data). For checking "what would have happened on Tuesday?", you want what would have actually happened — Dhan historical wins.
>
> For checking "what would have happened on Tuesday AT MY SHOP" (with the lost boxes), you want live ticks. But that's NOT a recipe test — that's a logistics test.
>
> **Answer: BACKTESTING = use Dhan historical. LIVE simulation = use live ticks.**

---

## 🎯 The DIRECT answer

### For BACKTESTING (in your separate product): use **Dhan historical** — not live ticks

| Concern | Live tick capture | Dhan historical |
|---|---|---|
| Completeness | ❌ May have gaps from OUR network blips | ✅ Server-side complete |
| NSE alignment | ❌ Subject to sampling drift | ✅ Closer to NSE (reconciled overnight) |
| Reproducibility | ❌ Each tickvault run captures different ticks | ✅ Same data every fetch |
| Granularity needed for backtest | tick-level OVERKILL for 1m strategy | 1m is exactly what you need |
| Storage cost | Massive (every tick × every instrument) | Tiny (375 candles × instruments) |
| Latency artifacts | Includes OUR network delays | Idealized clean data |
| Speed of backtest | Slow (must replay millions of ticks) | Fast (4M candles per day) |
| Cross-day consistency | Different days have different network quality | All days same source |

**Winner for BACKTESTING: Dhan historical.** Hands down.

---

## 🔍 WHY this is the right answer (5 reasons)

### Reason 1 — Backtest needs CLEAN data, not realistic data

Backtest goal: "Does this strategy work if it had been running on Tuesday's market?"

**Confounding factor:** if your live tick capture missed 5% of ticks on Tuesday, your backtest shows that strategy did poorly — but that's because YOUR DATA was incomplete, NOT because the strategy is bad.

**Dhan historical removes the confounder** — you get complete, reconciled data.

### Reason 2 — Reproducibility matters

You want to run the same backtest 10 times and get IDENTICAL results. Live ticks captured by tickvault could differ between days (different network quality). Dhan historical is fetched on-demand → always the same.

### Reason 3 — Cross-day comparability

Comparing strategy on Monday vs Tuesday vs Wednesday:
- Live ticks: each day has different captured-tick quality
- Dhan historical: same source, same quality, comparable

### Reason 4 — Storage + speed

For a strategy backtested over 1 year:
- Live ticks: ~6 TB of raw tick data per year
- Dhan historical 1m: ~50 GB per year
- 120× smaller storage; 100× faster to replay

### Reason 5 — Industry standard

Every professional backtest platform (QuantConnect, TradingView strategy tester, Zerodha Streak, Algorithmica) uses HISTORICAL data, not live tick replay. There's a reason.

---

## ⚠️ WHEN live ticks are better (the 1 case)

### Use live ticks ONLY for: simulating REAL operational conditions

If you want to answer: "What would have happened IF our system had been running with all its imperfections — missed ticks, network blips, latency?"

Then live ticks are the right source — because they include those imperfections.

**But this is OPS SIMULATION, not BACKTESTING.**

Workflow:
1. **Backtest in Groww-product** with Dhan historical → "does the strategy work?"
2. **Paper trade on tickvault Dhan-live** for 1 week → "does it work under our system's imperfections?"
3. **Go live** with confidence

---

## 📊 The 3-stage strategy validation pipeline (recommended)

```
┌──────────────────────────────────────────────────────────────┐
│  STAGE 1 — BACKTEST (in separate Groww product)              │
│  Source: Dhan historical /v2/charts/intraday                  │
│  Goal: Does the strategy LOGIC work?                          │
│  Duration: hours to days                                       │
│  Verdict: pass → Stage 2; fail → revise strategy              │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│  STAGE 2 — PAPER TRADE (in tickvault, dry_run=true)          │
│  Source: tickvault live ticks (Dhan WS)                       │
│  Goal: Does the strategy work UNDER OUR SYSTEM?               │
│  Duration: 1-4 weeks                                           │
│  Catches: slippage, network blips, latency, edge cases        │
│  Verdict: pass → Stage 3; fail → fix infra or revise          │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│  STAGE 3 — LIVE (in tickvault, dry_run=false)                 │
│  Source: live Dhan WS + Dhan order API                        │
│  Goal: Make real money                                         │
│  Monitor: continuously; halt on anomaly                       │
└──────────────────────────────────────────────────────────────┘
```

---

## 🎯 Why GROWW vs DHAN data matters for backtesting

Operator mentioned: "live ticks which will happen from Groww and Dhan."

**Recommendation:** for backtesting, use Dhan historical EVEN if you have Groww data.

**Why:**
- You TRADE through Dhan → live execution uses Dhan
- Dhan historical = same-broker baseline
- Strategy tested on Dhan historical → expected to behave identically in Dhan live
- ZERO porting drift

If you use Groww data for backtesting and Dhan for live:
- Backtest shows +₹5,000 P&L
- Live shows +₹4,950 P&L (1% slippage due to broker drift)
- Strategy looks "broken" when it's actually fine

**Stick with one broker source.** Dhan historical for backtest, Dhan live for execution.

---

## 🏗️ Practical recommendation

### For your separate backtesting product:

1. **Build a Dhan historical fetcher** (similar to tickvault's post-market heart piece)
   - Fetches 1m candles for any (security_id, date-range)
   - Stores locally for fast replay
   - Same idempotency, same retry logic

2. **Avoid raw tick replay** — overkill for 1m strategy

3. **Match the tickvault data schema** — same fields (OHLC + volume + OI + pcts)
   - When strategy ports to tickvault live, same code paths

4. **Document the slippage budget** — even with Dhan historical, expect 0.1-0.5% drift between backtest and live due to:
   - Order execution timing (you can't model exact fill timing in backtest)
   - Bid/ask spread (backtest assumes mid; live trades at bid or ask)
   - Market impact (large orders move the market)

### For your tickvault live system:

1. **Live ticks are for EXECUTION**, not for strategy R&D
2. **Live ticks ARE saved** (to QuestDB ticks table for forensic analysis)
3. **Live ticks ARE the source-of-truth for what your system saw** (audit)
4. **NEVER use live ticks for backtesting** — wrong source-of-truth

---

## 📊 Updated FOREVER charter implication

Per `operator-charter-forever.md`, backtesting is a SEPARATE product. This plan confirms:

| Concern | tickvault role | Backtest product role |
|---|---|---|
| Run finalized strategy in live market | ✅ | ❌ |
| Use Dhan historical for cross-verify | ✅ post-market | ❌ |
| Use Dhan historical for strategy R&D | ❌ | ✅ |
| Use Dhan live ticks | ✅ live execution + audit | ❌ |
| Paper trade strategy | ✅ Stage 2 of pipeline | ❌ |

**Separation maintained.** No backtest concept in tickvault.

---

## 🚨 Worst-cases for backtest data sourcing (W206-W210)

These apply to your SEPARATE backtest product (not tickvault), but documenting here for completeness:

| # | Scenario | Defense |
|---|---|---|
| W206 | Backtest passes but live fails due to slippage | Stage 2 paper trade catches |
| W207 | Dhan historical has bug (Dhan-side data error) | Cross-check with bhavcopy daily |
| W208 | Live tick capture has gaps; you use captures for backtest = wrong | DON'T USE captures for backtest |
| W209 | You use Groww historical for backtest, trade on Dhan | Switch to Dhan-historical for consistency |
| W210 | Backtest replay 1m candles miss tick-level events (e.g., flash crash + recovery within 1 min) | If strategy needs sub-minute granularity, increase to 5-sec candles — but rare |

---

## 📋 Discussion items

### D1 — Recommend a fast Dhan historical fetcher design for backtest product?

If operator wants, I can sketch the backtest product's data layer. **My vote:** wait — separate product. Out of scope for tickvault.

### D2 — Should tickvault EXPORT its captured live ticks for backtest use?

We have raw ticks in QuestDB (the `ticks` table) and `data/ws_wal/`. These COULD be used for backtest BUT shouldn't be (per Reason 1 above).

**My vote:** keep them for AUDIT only. Don't promote as "backtest data source."

### D3 — Should tickvault's post-market historical fetch ALSO populate backtest product?

We fetch Dhan historical every day at 15:31 IST for cross-verify. Could we send a copy to operator's backtest product?

**Pro:** one fetcher feeds both systems.
**Con:** coupling between two products.

**My vote:** YES — backtest product reads from tickvault's QuestDB `historical_candles` table. Simple, clean.

### D4 — Document the 3-stage validation pipeline as a permanent rule

Add to `.claude/rules/project/` so future Claude sessions know the workflow.

**My vote:** YES.

---

## 🎤 Direct answer (final)

**Operator's question:** "for backtesting, is it better to use live ticks (from Groww and Dhan) or historical data from Dhan?"

**My direct answer:**

| Question | Answer |
|---|---|
| For backtesting | **Use Dhan historical** |
| Why not live ticks | Live ticks have YOUR network gaps; historical is server-side complete |
| Why not Groww | Same-broker discipline; backtest with Dhan, execute with Dhan, no porting drift |
| Granularity needed | 1m is fine for most strategies (matches Dhan historical) |
| When to use live ticks | ONLY for Stage 2 paper trade in tickvault (operational simulation) |
| Storage saved | 120× smaller than raw tick replay |
| Speed gain | 100× faster backtest replay |

**Bottom line:** Dhan historical for the LOGIC test; Dhan live ticks (in tickvault paper trade) for the SYSTEM test. Don't mix the two.

5 NEW worst-cases W206-W210 added.

**Grand total worst-case paths: 421.**

Discussion mode continues. NO IMPLEMENTATION.
