# Topic — Why Dhan Has NSE-Canonical 1-Min Data + Can We Get It Too?

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** NSE market structure facts > this file.
> **Trigger:** Operator question 2026-05-12 20:25 IST: "Why does Dhan have real-time NSE 1-min data? How do they do it? Can WE fetch the same?"

---

## 🚗 Auto-Driver Story

> Sir, NSE is a private factory making juice. Each box of juice = one tick. They have FIVE different types of customers:
>
> 1. **Crore/year customers (institutional):** get the FULL FACTORY OUTPUT — every box, real-time
> 2. **Broker members (Dhan, Zerodha, Groww):** get the full factory output (because they're factory MEMBERS — pay membership fees)
> 3. **Third-party data resellers (GDF, TrueData):** get full output + repackage and resell
> 4. **Retail individuals:** get a SAMPLED subset via broker apps + EOD bhavcopy CSV
> 5. **Casual visitors (nseindia.com):** get LAGGED public charts
>
> Dhan is Category 2 — they're a BROKER MEMBER of NSE. They pay membership + get full TBT (tick-by-tick) feed.
>
> **Can YOU get Category 2 access? NO** (unless you become a broker yourself — ₹lakhs setup + licensing).
>
> **Can you get Category 3? YES** — pay GDF/TrueData ~₹2,000-5,000/month.

---

## 🏭 The NSE data access hierarchy

```
┌──────────────────────────────────────────────────────────────┐
│  TIER 1 — INSTITUTIONAL DIRECT                                │
│  - Co-location servers at NSE data centre                     │
│  - TBT (Tick-By-Tick) feed via NEAT/Multicast                │
│  - Sub-millisecond latency                                    │
│  - Cost: ₹2-10 lakh/month                                     │
│  - Examples: HFT firms, prop desks, hedge funds              │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼ (broker membership)
┌──────────────────────────────────────────────────────────────┐
│  TIER 2 — BROKER MEMBERS (Dhan, Zerodha, Groww, Upstox)       │
│  - Full TBT feed via NSE membership                           │
│  - Server-side complete tick storage                          │
│  - Build 1-min, 5-min, etc. candles for THEIR clients         │
│  - Cost: NSE membership ₹lakhs + annual fees                  │
│  - Pass DOWNSAMPLED data to retail clients (us)               │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼ (sell access)
┌──────────────────────────────────────────────────────────────┐
│  TIER 3 — THIRD-PARTY DATA RESELLERS                           │
│  - GlobalDataFeeds (GDF), TrueData, InvestFly, Algomojo       │
│  - Buy TBT from NSE; resell to retail with API                │
│  - More complete than retail brokers (less sampling)          │
│  - Cost: ₹2,000 to ₹15,000/month depending on coverage       │
│  - Examples: GDF gives Cap/SubCap NIFTY at lower latency      │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼ (downsampled via API)
┌──────────────────────────────────────────────────────────────┐
│  TIER 4 — RETAIL BROKER APIs (us, today)                       │
│  - Dhan WS: sampled snapshots (100-250ms)                     │
│  - Dhan REST historical: server-side complete 1-min candles   │
│  - Cost: FREE with broker account                             │
│  - Quality: 99.9% of NSE truth, ~100ms-1s latency             │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼ (anti-bot, lagged)
┌──────────────────────────────────────────────────────────────┐
│  TIER 5 — NSE PUBLIC WEBSITE (nseindia.com)                    │
│  - JSON endpoints scraped by retail traders                   │
│  - Cloudflare anti-bot protection                             │
│  - Rate-limited heavily                                       │
│  - Not real-time (often 15-30 min delayed)                    │
│  - NOT FIT for live trading                                   │
└──────────────────────────────────────────────────────────────┘
```

---

## 🎯 So how does Dhan have NSE-canonical 1-min data?

**They're a BROKER MEMBER (Tier 2).** Specifically:

1. Dhan is a SEBI-registered NSE trading member
2. They paid NSE membership fees (₹lakhs setup + annual)
3. They receive NSE's TBT feed (tick-by-tick) at exchange-level fidelity
4. Their servers receive EVERY trade in real-time (or near real-time, ~10ms latency)
5. Their server-side tick storage has the COMPLETE picture
6. When you call `/v2/charts/intraday`, they build the 1-min candle from THEIR complete store
7. After the minute closes, they finalise the high/low/close to match NSE official

**Why their REST endpoint refreshes 1 min later:**
- Live WS: they sample (~100-250ms) to keep retail bandwidth manageable
- The candle for 09:18:00–09:18:59 isn't FINAL until 09:19:00:00
- Their internal store has all ticks; the REST endpoint waits until the minute closes to publish the final candle
- That's why operator sees "Dhan chart refreshes 1-min after the minute closes"

---

## 🎯 Can WE get the same access?

| Tier | Option | Cost | Realistic? |
|---|---|---|---|
| **Tier 1 (Institutional direct)** | Co-location + NSE membership | ₹2-10 lakh/month | ❌ NO — far beyond our budget |
| **Tier 2 (Become broker member)** | Apply for NSE membership | ₹50 lakh+ setup | ❌ NO — out of scope |
| **Tier 3 (Buy from reseller)** | GlobalDataFeeds, TrueData, etc. | ₹2,000 - ₹15,000/month | ✅ **POSSIBLE** if budget allows |
| **Tier 4 (Stay with Dhan)** | Current setup | included | ✅ **CURRENT** |
| **Tier 5 (Scrape NSE public)** | nseindia.com JSON | free | ⚠️ Anti-bot, lagged, NOT viable for live |

---

## 🎯 Tier 3 details — third-party feed providers

### Major Indian retail-grade NSE feed resellers

| Provider | Coverage | Cost (approx) | Latency | API quality |
|---|---|---|---|---|
| **GlobalDataFeeds (GDF)** | NIFTY/BANKNIFTY F&O + selected stocks | ₹2,000-5,000/month | ~50-200ms | Good Java/Python SDKs |
| **TrueData** | Full F&O + equities + commodities | ₹3,000-15,000/month | ~50-100ms | REST + WS APIs |
| **InvestFly** | Limited NSE coverage | ₹1,500-3,000/month | ~200ms | Basic |
| **Algomojo** | Bundled with their algo platform | varies | ~100-200ms | Tied to Algomojo bot |
| **Kotak Neo / Zerodha Kite** | their own client API | included with account | ~100ms | only their clients |

### What you GAIN with Tier 3

| Benefit | Detail |
|---|---|
| Lower-latency ticks | 50-100ms vs Dhan's 100-250ms |
| Fewer missed ticks | TBT-like fidelity |
| Closer to NSE canonical | Better cross-verify |
| Multi-broker independence | Not tied to one broker's data |

### What you LOSE / RISKS

| Risk | Detail |
|---|---|
| **Cost** | ₹2K-5K/month adds 40-100% to our ₹5K AWS budget |
| **Vendor lock-in** | If GDF discontinues, migration painful |
| **Still not literal NSE** | Tier 3 is a RESELLER; depends on their connection quality |
| **Two data sources to reconcile** | GDF for live, Dhan for orders — cross-verify becomes more complex |
| **Same 1-min minute boundary issue** | Even GDF candle for 09:18 isn't FINAL until 09:19:00 (physics) |

---

## 🎯 The PHYSICS constraint (important)

Even if you had institutional Tier 1 access, **you cannot have a "final" 1-minute candle BEFORE the minute closes**.

```
09:18:00.000   Minute starts
09:18:00.001   First tick of minute lands
...
09:18:59.999   Last tick of minute could happen
09:19:00.000   Minute ENDS — only NOW can you say "high = 135, close = 130"
```

**Reason:** until 09:19:00, the minute is still in progress. A tick could arrive at 09:18:59.999 that's higher than anything seen so far.

So "1-min candle for 09:18" is INHERENTLY 60 seconds after-the-fact.

**Operator's earlier observation** ("Dhan refreshes 1-min view after 1 minute") is partly this PHYSICS — not a Dhan-specific lag. Even with Tier 1 institutional access, you'd see the same "after-minute-closes-it-finalizes" behavior.

---

## 🎯 What's REAL-TIME vs WHAT'S NOT

| Data | When available | Lag |
|---|---|---|
| Individual ticks (LTP, trade prints) | Real-time via WS | 10ms-300ms depending on tier |
| Best bid/ask | Real-time via WS | Same |
| Order book depth | Real-time via WS | Same |
| 1-minute candle (closed) | After minute ends | 1 minute (PHYSICS) |
| 5-min, 15-min, etc. candles | After respective bucket closes | Their duration |
| Daily candle (1d) | After 15:30 IST | Until 18:00 IST EOD reconciliation |
| Bhavcopy CSV | Next morning ~08:00 IST | ~17 hours |

**Live trading happens at TICK level, not candle level.** Strategy decides on tick LTP + indicator state.

---

## 🎯 The PRACTICAL recommendation for tickvault

### Option A — Stay with Dhan (current)

| Pro | Con |
|---|---|
| ₹0 extra cost | 99.9% of NSE truth (not 100%) |
| Already integrated | Dependent on Dhan service quality |
| Cross-verify catches drift daily | Sampling drift may affect strategy in volatile periods |

### Option B — Add GlobalDataFeeds (GDF) for cross-verification only

| Pro | Con |
|---|---|
| Tier 3 independent check vs Dhan | ₹2K-5K/month |
| Better catch silent Dhan drift | Two data sources to maintain |
| Weekly drift digest more reliable | Cross-verify code becomes complex |

### Option C — Replace Dhan with TrueData for live, keep Dhan for orders

| Pro | Con |
|---|---|
| Higher-fidelity live data | ₹3K-15K/month |
| Less Dhan-WS latency | Major code refactor (different WS protocol) |
| Still need Dhan for orders (so 2 vendors) | More vendor risk |

### Claude's recommendation

**Stay with Option A for now.** Reasons:
1. Cross-verify daily catches Dhan-side drift
2. Bhavcopy daily catches EOD discrepancies
3. For 1-min strategies on liquid F&O, Dhan's ~99.9% is sufficient
4. ₹2K-5K/month for GDF is significant for retail trading P&L

**Revisit when:** backtest shows your strategy is sensitive to data drift (paper-trade reveals >0.5% P&L difference attributable to data quality).

---

## 🏗️ If operator chooses Option B (GDF for cross-verify)

Architecture:

```
┌──────────────────────────────────────────────────────────────┐
│  LIVE PATH (orders + strategy)                                │
│  Dhan WS → tickvault aggregator → strategy → Super Order      │
└──────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────┐
│  CROSS-VERIFY PATH (audit, GDF-backed)                        │
│  GDF API ───────────▶ historical_candles_gdf table            │
│                                                                │
│  Daily 17:25 IST cross-verify:                                │
│    candles_1m_shadow (live agg) vs                            │
│    historical_candles (Dhan REST) vs                          │
│    historical_candles_gdf (GDF API)                           │
│                                                                │
│  ZERO tolerance on all 3 cross-matches                        │
└──────────────────────────────────────────────────────────────┘
```

Cost: ₹2,000-5,000/month for GDF + integration code (~500 LoC).

---

## 🚨 NEW worst-cases (W236-W240)

| # | Scenario | Defense |
|---|---|---|
| W236 | Dhan service degraded → our data drifts vs NSE canonical | Bhavcopy daily catches; if Option B implemented, GDF catches earlier |
| W237 | GDF (if subscribed) becomes unreliable | Fall back to Dhan-only cross-verify |
| W238 | All three (Dhan + GDF + bhavcopy) disagree | Investigate; manual operator review |
| W239 | Trying to scrape nseindia.com → IP banned | NEVER scrape NSE public site for live data |
| W240 | Switch broker (Dhan → Zerodha) → data quality changes | New broker integration + cross-verify recalibration |

---

## 📊 Total worst-case coverage: 451

```
Prior 446 + NEW NSE data access (W236-W240) = 451
```

---

## 🎤 Direct answers to operator

### Q1: "Why does Dhan have NSE-canonical 1-min data?"

**A:** Dhan is a SEBI-registered NSE BROKER MEMBER. They pay membership fees to NSE → receive full Tick-By-Tick (TBT) feed at exchange fidelity. Their server-side tick storage has the COMPLETE picture. They build 1-min candles from their complete store.

### Q2: "How are they doing it?"

**A:** Three-step pipeline:
1. NSE TBT feed → Dhan's server-side tick store (every trade captured)
2. Server-side aggregator builds 1-min candles when minute closes
3. REST endpoint serves these reconciled candles to clients

### Q3: "Can WE fetch directly from NSE in real-time?"

**A:** **NOT DIRECTLY.** NSE doesn't sell retail-tier real-time feeds. Options:
- Become NSE broker member (₹50 lakh+ setup) — NO
- Buy from Tier 3 reseller (GDF/TrueData ₹2K-15K/month) — YES if budget allows
- Stay with Dhan + cross-verify (current) — RECOMMENDED for retail budget

### Q4: "If there is a way, how can we do it?"

**A:** Subscribe to GlobalDataFeeds (or TrueData). They provide WS/REST API similar to Dhan but with closer-to-NSE-canonical data. ₹2K-5K/month. Integration ~500 LoC.

### The PHYSICS gotcha

Even with Tier 1 institutional access, **a 1-minute candle is FUNDAMENTALLY 60 seconds after the minute starts**. The "lag" operator observes when Dhan refreshes the 1-min view is partly PHYSICS, not Dhan-specific.

Live trading decisions are at TICK level (LTP, depth), not 1-min closed candle level.

---

## 🚗 Final auto-driver summary

> Sir, you asked: "How does Dhan get NSE's exact data?"
>
> ANSWER: Dhan is a BROKER MEMBER of NSE. They pay NSE membership → receive FULL TBT (tick-by-tick) feed. They aggregate server-side. They sell DOWNSAMPLED version to retail (us).
>
> "Can we get the same access?"
>
> ANSWER: NO directly (NSE doesn't sell retail tier). YES via TIER 3 RESELLERS:
> - GlobalDataFeeds: ₹2K-5K/month
> - TrueData: ₹3K-15K/month
> - Still goes through a reseller, not literal NSE
>
> "Should we?"
>
> ANSWER: NOT NOW. Stay with Dhan. Daily cross-verify catches drift. If your backtest reveals strategy is data-quality-sensitive, revisit then.
>
> ALSO: even institutional Tier 1 access has the SAME 1-minute "completion lag" because physics — a 1-min candle needs 60 seconds to complete.

5 NEW worst-cases W236-W240. **Grand total: 451.**

Discussion mode continues. NO IMPLEMENTATION.
