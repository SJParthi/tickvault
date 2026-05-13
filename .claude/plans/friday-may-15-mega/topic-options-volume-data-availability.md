# Topic — Options Volume Data Availability (the truth about 09:00 vs 09:15)

**Created:** 2026-05-12 morning IST
**Trigger:** Operator: "for top volume desc — do we need yesterday's volume or fresh today? Will we receive options data from 09:00 or only 09:15? How does Dhan calculate cumulative volume?"
**Status:** Critical clarification — fixes a timing error in my previous design

---

## 🎯 The honest short answers

| Question | Answer |
|---|---|
| Do we need yesterday's volume for "top volume desc"? | **NO** — volume is fresh-today, resets to 0 every morning |
| Do options trade from 09:00 or 09:15 IST? | **09:15 ONLY** — F&O has NO pre-open session (only equities have 09:00-09:08 pre-open) |
| When do we get first options volume data? | **09:15:00.000 IST** — first option tick of the day |
| When can we first rank by top-volume? | **09:16:00 IST** — after first 1-min window (`movers_1m`) is complete |
| How does Dhan calculate volume? | **Cumulative-day** — bytes 22-25 of Quote/Full packet = total volume traded since 09:15 today, resets next morning |

---

## 📚 NSE trading hours — the truth

| Segment | Pre-open | Continuous | Close |
|---|---|---|---|
| **Equities (NSE_EQ)** | 09:00 - 09:08 | 09:15 - 15:30 | 15:40 (closing session) |
| **F&O (NSE_FNO)** | **NONE** | **09:15 - 15:30** | n/a |
| Currency (NSE_CURRENCY) | None | 09:00 - 17:00 | n/a |
| Commodity (MCX_COMM) | None | 09:00 - 23:30 | n/a |

**Critical point: F&O has ZERO trading 09:00-09:14:59.** No quotes, no trades, no volume. The pre-open buffer (09:00-09:12) we built captures EQUITY closes only — used to anchor stock-F&O ATM at 09:13 IST.

For OPTIONS volume — **we wait until 09:15:00.000 sharp**.

---

## 🔢 How Dhan's `volume` field works (bytes 22-25 of Quote/Full packet)

```
   09:15:00.000  →  volume = 0
   09:15:00.123  →  volume = 1,200    (first trades absorbed)
   09:15:01.000  →  volume = 8,400
   09:15:30.000  →  volume = 152,000
   09:16:00.000  →  volume = 487,000  (1 min of trading)
   ...
   10:00:00.000  →  volume = 4,500,000 (cumulative)
   ...
   15:30:00.000  →  volume = 8,200,000 (final daily total)
   
   ─── next trading day ───
   
   09:15:00.000  →  volume = 0  (RESET)
```

**Key facts:**

1. Volume is **cumulative-since-09:15-today** (monotonically increasing, per `VOLUME-MONO-01` guard)
2. Resets to **0 at 09:15:00 each new trading day**
3. Same field structure for indices (IDX_I), stocks (NSE_EQ), options (OPTIDX/OPTSTK) — all cumulative-day
4. We **do NOT need yesterday's volume** for ranking — today's cumulative is self-contained
5. % change DOES need yesterday's close (different field — `close` at bytes 38-41 of Quote / 50-53 of Full)

---

## ⏰ The CORRECTED timeline (fixing my earlier mistake)

I previously said the depth cold-start window is 09:00-09:16 (16 minutes). **That was wrong.** Real timeline:

```
   08:30 IST     09:00 IST          09:15 IST              09:16 IST
   AWS boot      WS pool wakes      F&O MARKET OPENS       First movers_1m row
                                    (first option tick)    (first ranking possible)
       │             │                    │                       │
       ▼             ▼                    ▼                       ▼
   ┌───────────┬─────────────────┬────────────────────────┬─────────────────┐
   │ Boot 3min │ WS conns open   │ F&O ticks start flowing│ TOP-VOLUME      │
   │ Done by   │ Subscribe to    │ Volume field grows     │ RANKING starts  │
   │ 08:33     │ 11K instruments │ from 0                 │                 │
   │           │ (whole universe)│                        │ Depth-20: top   │
   │           │                 │ But NO ticks until     │ 250 by volume   │
   │           │ 5-LEVEL depth   │ 09:15:00 sharp         │                 │
   │           │ from main feed  │                        │ Depth-200: top  │
   │           │ already active  │ Volume monotonicity    │ 5 by volume     │
   │           │ via Full mode   │ guard activates        │                 │
   │           │                 │                        │ Swap20+Swap200  │
   │           │ Depth-20 + 200  │                        │ commands fire   │
   │           │ in DEFERRED     │                        │                 │
   │           │ state (or ATM   │                        │                 │
   │           │ bootstrap if    │                        │                 │
   │           │ we configure)   │                        │                 │
   └───────────┴─────────────────┴────────────────────────┴─────────────────┘
   
   Cold-start window for top-volume ranking:
   • Earliest possible: 09:16:00 (1 min of data)
   • Smoother: 09:20:00 (5 min of data)
   • Most stable: 09:30:00 (full opening volatility settled)
```

**Cold-start window is REALLY 09:00 → 09:16 (16 min)** but the BLOCKING window is just **09:15 → 09:16 (1 minute)** — before 09:15 there's no F&O data anyway.

---

## 🎯 Two ways to interpret "top volume"

The word "volume" is ambiguous. Pick which one:

| Interpretation | What it picks | Pros | Cons |
|---|---|---|---|
| **A — Cumulative-day volume** | Top 250 by `sum(volume_today)` | Stable, captures full-day winners | Slow to react. At 09:16 it equals "first minute volume" anyway |
| **B — Rolling 60s window** ✅ | Top 250 by `sum(volume_in_last_60s)` | Reactive, captures CURRENT hot contracts | More re-ranking churn |
| C — Hybrid (cumulative + recent weight) | Weighted combo | Smooths transitions | Complex tuning |

**`movers_1m` materialized view = Option B (rolling 60s).** Already exists, already correct for what you described.

**My recommendation: Option B (rolling 60s).** Matches operator's stated intent ("WHICH options are being traded the MOST RIGHT NOW") and matches existing infrastructure.

If you want Option A (cumulative-day): we'd need a different mat view OR aggregate `movers_1m` rows since 09:15.

---

## 📊 The data lifecycle within one trading day

```
   09:15:00     09:15:30     09:16:00     09:20:00     15:30:00     16:00:00
   F&O OPEN     30 sec in    1 min in     5 min in     CLOSE        Audit
       │            │            │            │            │            │
       ▼            ▼            ▼            ▼            ▼            ▼
   ┌────────┬─────────────┬──────────────┬─────────────┬────────────┬────────────┐
   │volume=0│ tick stream │ FIRST        │ Reliable    │ FINAL      │ Cross-     │
   │        │ flowing.    │ movers_1m    │ ranking.    │ daily      │ verify vs  │
   │        │ Per-tick    │ row written. │ Top-N stable│ volume     │ Dhan REST  │
   │        │ volume      │              │             │ in QuestDB │            │
   │        │ accumulates │ Ranking      │ Continue    │            │            │
   │        │             │ POSSIBLE.    │ re-ranking  │            │            │
   │        │ Aggregator  │              │ every 60s   │            │            │
   │        │ builds      │ Swap20/200   │             │            │            │
   │        │ candles_1m  │ FIRES.       │ Same        │            │            │
   │        │             │              │ algorithm.  │            │            │
   │        │             │ Depth-20:    │             │            │            │
   │        │             │ 250 contracts│             │            │            │
   │        │             │ Depth-200:   │             │            │            │
   │        │             │ 5 contracts  │             │            │            │
   └────────┴─────────────┴──────────────┴─────────────┴────────────┴────────────┘
   
   Telegram pings:
   📱 09:15:30  "Market open — streaming"      (existing)
   📱 09:16:00  "Depth auto-ranked top-N"      (NEW — Phase 0.5 task)
   📱 every 60s  silent unless big change      (rolling re-rank)
```

---

## ⚙️ Updated config recommendation

```toml
[depth_20.dynamic.universe]
exchange_segments = ["NSE_FNO"]      # ✅ excludes SENSEX (BSE)
instrument_types = ["OPTIDX"]         # ✅ index options only (NIFTY + BANKNIFTY)
min_liquidity_volume = 0              # NO floor — let top-N speak for itself
rerank_metric = "volume_desc"         # ← CHANGE THIS (was "change_pct_desc")
window_secs = 60                      # rolling 60-sec window
```

**`min_liquidity_volume = 0`** because the top-N sort already enforces volume ordering. A non-zero floor only matters if you want to filter low-volume noise — at top-250 you're already in the busiest contracts.

---

## 🚨 Special cases to handle

| Case | Behavior | Detection |
|---|---|---|
| 09:15:00.000 → 09:16:00.000 (cold-start) | No `movers_1m` data yet → depth-20/200 in ATM bootstrap mode | First swap fires at 09:16:00 |
| Expiry Thursday (BANKNIFTY) | Volume concentrates in current-week options → top-N is heavily expiry-weighted | normal — ranking reflects reality |
| Half-day Diwali Muhurat session | Trading is 18:15 - 19:15 IST | scheduler reads `nse_holidays` config; same algorithm in shifted window |
| Holiday | No ticks anywhere | depth pipeline detects 0 ticks → no swap commands |
| First tick of day with volume already non-zero | Bug — shouldn't happen on real F&O segment | `VOLUME-MONO-01` would catch but it's per-(sid,segment) not first-tick |

---

## 🛠 Updated Friday Phase 0.5 task

Update **Item 0.5.X (verify + finalize dynamic depth pipeline)** to include:

| # | Sub-task | LoC |
|---|---|---|
| 1 | Verify `depth_dynamic_pipeline_v2::spawn` wired in `main.rs` boot | 20 |
| 2 | Flip `rerank_metric` config from `change_pct_desc` → `volume_desc` | 5 |
| 3 | Add 1.1× hysteresis rule in selector | 50 |
| 4 | Add `DepthAutoRanked` notification event with rich Telegram | 80 |
| 5 | Add ratchet test pinning 5×50=250 + 5×1=5 capacity | 30 |
| 6 | Add boot-state machine: WS pool DEFERRED → ATM bootstrap → top-N swap at 09:16:00 | 50 |
| 7 | Document 09:00 vs 09:15 distinction in `live-market-feed-subscription.md` | (docs) |

**Total: ~235 LoC.** Friday Session 6.

---

## 🚖 Auto-driver / dumb-kid summary

> "Sir, great question — and I had it slightly wrong before.
>
> **Stocks** = 09:00 to 09:08 pre-open auction + 09:15 continuous → 15:30.
> **Options + Futures** = NO pre-open. Trading starts 09:15:00 sharp.
>
> So for options volume:
> - 09:00 to 09:14:59 → **ZERO data** (F&O market closed)
> - 09:15:00 sharp → first option tick, volume starts from 0
> - 09:15:30 → 30 sec of volume accumulated
> - 09:16:00 → first 1-minute window complete → **FIRST TOP-VOLUME RANKING POSSIBLE**
> - Then every 60 sec → re-rank
>
> Volume is fresh-today, resets every morning. NO need for yesterday's volume.
>
> Recommendation: rolling 60-sec window (already exists as `movers_1m`). Captures who's HOT RIGHT NOW, not who was hot all day."

---

## 🎤 Now your call

| Pick | Means |
|---|---|
| **A** | Accept all (rolling 60s + volume_desc + 1.1× hysteresis + ATM cold-start + 09:16 first swap) |
| **B** | Same as A but cumulative-day volume instead of rolling 60s |
| **C** | Show me how `movers_1m` is built (deep-dive code flow) |
| **D** | Different angle — ask me X |

Floor's yours dude. ☕
