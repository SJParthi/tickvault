# Topic — Top-N Volume Dynamic Depth Subscription Plan

**Created:** 2026-05-12 morning IST
**Trigger:** Operator: "after 9 AM ticks flow → find top-N volume options → subscribe depth-20 (5×50=250) and depth-200 (5×1=5). NIFTY+BANKNIFTY only (exclude SENSEX as it's BSE). Tell me the plan clearly."
**Mode:** Design clarification (no implementation)
**Status:** Code partially exists, needs operator decisions + boot wiring

---

## 🎯 The big picture (auto-driver level)

> "Right now: depth-20 / depth-200 subscribe to ATM±N strikes (static, based on pre-open price).
>
> Operator's idea: AFTER 9:15 AM ticks flow → look at WHICH option contracts are actually being traded the most (top volume) → swap depth-20 (250 contracts) + depth-200 (5 contracts) to those.
>
> Like a taxi driver who first parks at the predicted-busy street, then after 5 minutes of watching where the actual crowd is → moves to where the crowd actually IS."

---

## ✅ What ALREADY EXISTS in code today

| Component | Status | File |
|---|---|---|
| Dynamic selector module | ✅ EXISTS | `crates/core/src/instrument/depth_dynamic_top_volume_selector.rs` |
| Pipeline v2 (unified depth-20 + depth-200) | ✅ EXISTS | `crates/app/src/depth_dynamic_pipeline_v2.rs` |
| Config schema for cohort + metric + window | ✅ EXISTS | `config/base.toml` lines 308-328 |
| `movers_1m` materialised view in QuestDB | ✅ EXISTS | from main feed aggregator |
| `Swap20` / `Swap200` zero-disconnect command channel | ✅ EXISTS | `depth_connection.rs` |
| ErrorCode `DEPTH-20-DYN-03` / `DEPTH-200-DYN-01` (empty set detection) | ✅ EXISTS | `wave-5-error-codes.md` |

---

## ⚙️ Current config (config/base.toml lines 308-328)

```toml
[depth_20.dynamic]
conns = 5
sids_per_conn = 50         # 5 × 50 = 250 instruments ✅

[depth_20.dynamic.universe]
exchange_segments = ["NSE_FNO"]    # ✅ excludes SENSEX (BSE)
instrument_types = ["OPTIDX"]       # ✅ index options only
min_liquidity_volume = 10000        # floor filter
rerank_metric = "change_pct_desc"   # ⚠️ currently change_pct, NOT volume
window_secs = 60                    # 1-min window

[depth_200.dynamic]
conns = 5
sids_per_conn = 1          # 5 × 1 = 5 instruments ✅

[depth_200.dynamic.universe]
# (same as depth_20)
```

---

## 🚨 Mismatch between config + operator intent

| Operator said | Config today |
|---|---|
| "top N **VOLUME** options" | `rerank_metric = "change_pct_desc"` (sorts by price-change %, NOT volume) |

**Decision needed:** what should `rerank_metric` be?

| Option | What it ranks by | When you'd want this |
|---|---|---|
| **`volume_desc`** | Highest absolute volume | "Who's getting traded the most right now?" — operator's stated intent |
| `change_pct_desc` | Biggest % movers | "Who's moving the most?" (today's default) |
| `vol_x_change_desc` | Volume × abs(change%) hybrid | "Most volatile + active combo" |
| `oi_change_desc` | OI building fastest | "Where smart money is positioning" |

---

## 🎯 The 4 design decisions you need to make

### Decision 1: Ranking metric

**Recommendation: `volume_desc`** (matches your stated intent).

Trade-off:
- Volume-desc = stable, captures most-traded contracts (NIFTY ATM options + busy expiries)
- Change_pct-desc = volatile, can flip every 60 sec on small contracts moving 10%

### Decision 2: Cold start (09:00 → 09:16 — no volume data yet)

The window between WS pool open (09:00) and first 1-min volume window complete (~09:16) has NO `movers_1m` data. What do depth-20/200 subscribe to in this 16-min window?

| Option | Behavior | Pros | Cons |
|---|---|---|---|
| **A — ATM bootstrap** (recommended) | Use current ATM-based selection until 09:16, then swap to top-N | Always have depth coverage, smooth handover | 2 swap events at 09:16 (one for depth-20, one for depth-200) |
| B — Wait cold | No depth subscriptions until 09:16:30 | Cleanest startup | 16 min of no depth data — operator's "ATM" insight is lost |
| C — Yesterday's last-hour bootstrap | Use yesterday 14:30-15:30 volume top-N | Already-known busy contracts | Stale: today's volume distribution may differ; needs persistence |

**My recommendation: A (ATM bootstrap).** The depth conns subscribe to ATM-based contracts at 09:00 (their existing behavior), then swap to top-N volume at ~09:16:00.

### Decision 3: Re-rank frequency

| Option | Refresh | Swap traffic |
|---|---|---|
| 60 sec (current `window_secs`) | every minute | up to 250 swap events/min worst case (no hysteresis) |
| 5 min | every 5 min | low traffic, slower to track |
| **60 sec + hysteresis** (recommended) | every minute BUT only swap if new contract's vol > current × 1.1 | balances responsiveness + stability |

**My recommendation: 60 sec + hysteresis × 1.1.** Reduces swap thrashing by ~80%.

### Decision 4: Tie-breaking when many contracts have similar volume

When 1,000+ OPTIDX contracts on NSE_FNO have similar volumes, who wins the top-250 cut?

| Tie-breaker | Pro | Con |
|---|---|---|
| **Closer to ATM first** (recommended) | Most actionable for trading | Slight bias toward NIFTY (heaviest volume index) |
| Sooner expiry first | Most liquid contracts | Misses far-month early flows |
| Symbol alphabetical | Deterministic | Arbitrary |

**My recommendation: ATM-distance first, then sooner-expiry, then alphabetical.**

---

## 📊 The full timeline you'll see Friday morning

```
   09:00 IST           09:15 IST           09:16:00            09:17:00
   WS pool opens       Market opens        First 1-min          Re-rank
                                          movers_1m window
       │                   │                   │                   │
       ▼                   ▼                   ▼                   ▼
   ┌─────────────────┬─────────────────┬─────────────────┬─────────────────┐
   │ Depth-20        │ Live ticks flow │ Top-250 by       │ Re-rank checks  │
   │ subscribes to   │ Aggregator      │ volume picked    │ if new top-N    │
   │ ATM ± 24 strikes│ builds movers_1m│ from movers_1m   │ differs from    │
   │ (49 instruments │ from incoming   │                  │ current. If     │
   │ per underlying) │ ticks           │ Swap20 fires →   │ delta > 1.1× →  │
   │ × 2 underlyings │                 │ depth-20 conns   │ swap that conn  │
   │ = ~98 today     │                 │ swap to top-250  │                 │
   │                 │                 │                  │                 │
   │ Depth-200       │                 │ Swap200 fires →  │ Re-rank every   │
   │ subscribes to   │                 │ depth-200 conns  │ 60 sec from now │
   │ ATM CE + ATM PE │                 │ swap to top-5    │                 │
   │ × 2 underlyings │                 │                  │                 │
   │ = 4 today       │                 │                  │                 │
   └─────────────────┴─────────────────┴─────────────────┴─────────────────┘
   
   📱 PING 4 09:15:30: "Market open — streaming"
   📱 PING 5 09:16:00: "Depth-20 + Depth-200 swapped to top-volume contracts" (NEW)
   📱 (silent unless big change) every 60 sec re-rank
```

---

## 📱 Telegram for the 09:16 swap (NEW notification)

```
🎯 DEPTH AUTO-RANKED — top volume contracts subscribed

Time: 09:16:00 IST · 1-min volume window complete

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 DEPTH-200 (top 5 by volume)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  1. NIFTY 22700 CE   · vol 8.4M ·  (was ATM CE)
  2. NIFTY 22700 PE   · vol 7.9M ·  (was ATM PE)
  3. BANKNIFTY 48200 CE · vol 6.1M · (was ATM CE)
  4. BANKNIFTY 48200 PE · vol 5.8M · (was ATM PE)
  5. NIFTY 22800 CE   · vol 4.2M ·  NEW

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 DEPTH-20 (top 250 by volume)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  NIFTY options:        152 contracts
  BANKNIFTY options:     98 contracts
  Total:                250 / 250 ✅

  Volume range: 0.1M → 8.4M
  ATM±N coverage: NIFTY ± 12 strikes / BANKNIFTY ± 8 strikes

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔄 Re-ranking every 60 sec
   Next check: 09:17:00 IST
```

---

## 🤔 Open questions for you

| # | Question | My recommendation |
|---|---|---|
| **Q1** | Volume vs change_pct ranking? | Volume_desc (your stated intent) |
| **Q2** | Cold-start strategy? | ATM bootstrap → swap at 09:16 |
| **Q3** | Re-rank frequency? | 60 sec + 1.1× hysteresis |
| **Q4** | Tie-breaker? | ATM-distance, then expiry, then symbol |
| **Q5** | Include futures (FUTIDX) in universe? | NO — only OPTIDX (operator said "options") |
| **Q6** | Include MIDCPNIFTY / FINNIFTY index options? | NO — operator dropped these per Wave 5 |
| **Q7** | What if cohort has < 250 contracts? (low-volume day) | DEPTH-20-DYN-03 alert fires; subscribe what's available |
| **Q8** | What if a contract gets de-listed mid-day (expiry)? | Drop from set; next swap fills slot |

---

## ⚠️ The hidden problem you MAY not have considered

**The "depth conn 5 has only 1 instrument" issue for depth-200:**

If you subscribe top-5 by volume across BOTH NIFTY + BANKNIFTY → it's totally possible all 5 are NIFTY (NIFTY dominates volume by 3-4×). BANKNIFTY gets ZERO depth-200 coverage that day.

| Option | Behavior |
|---|---|
| **A — Open cohort** (recommended for retail) | Pure top-5 by volume. NIFTY-heavy days = NIFTY-dominated depth-200. Honest reflection of where action is. |
| B — Forced split (3 NIFTY + 2 BANKNIFTY) | Guaranteed coverage of both underlyings. Loses 1-2 slots to lower-volume contracts. |
| C — Per-underlying quota (top 3 NIFTY + top 2 BANKNIFTY) | Same as B but config-controlled |

**My recommendation: A (open cohort)** — operator's stated intent is "top volume." If NIFTY dominates today, that's where the action is. BANKNIFTY still gets depth-20 (~98 contracts), just not depth-200.

---

## 🛠 What's likely NOT yet wired (needs verification)

Even though code + config exist, these may need a Friday task:

| Item | Verify needed |
|---|---|
| `depth_dynamic_pipeline_v2::spawn` actually called in `main.rs` boot sequence | grep `main.rs` for it |
| `rerank_metric` config respected (currently `change_pct_desc`, may need `volume_desc`) | review selector code |
| `Swap20` / `Swap200` commands actually fire from pipeline output → depth_connection | trace the flow |
| Telegram event for "depth auto-ranked at 09:16" exists | search notification/events.rs |
| Cold-start ATM bootstrap → swap handover is clean (no double-subscribe) | code review |
| Hysteresis 1.1× rule exists | check selector code |

**Proposed task:** Friday Phase 0.5 Item 0.5.X = "**Verify + finalize dynamic depth pipeline**" (~200 LoC) to:
1. Confirm boot wiring
2. Flip `rerank_metric` to `volume_desc` if operator agrees
3. Add hysteresis 1.1× rule
4. Add `DepthAutoRanked` Telegram event
5. Add ratchet test pinning the 5×50=250 + 5×1=5 capacity

---

## 🚖 Auto-driver summary

> "Sir, you said: after 9:15 AM, look at WHICH options are actually being traded the most → subscribe depth-20 (250 of them) + depth-200 (5 of them).
>
> Good news: someone already designed this. Code exists. Config exists. Movers table exists.
>
> Bad news: 3 things need your decision:
> 1. Currently ranks by % CHANGE, you said VOLUME — flip the config?
> 2. From 9:00 to 9:15, no volume data yet — subscribe ATM first, then swap at 9:16?
> 3. NIFTY has 3× more volume than BANKNIFTY — pure top-5 means BANKNIFTY may get ZERO depth-200 some days. OK with that, or force split 3 NIFTY + 2 BANKNIFTY?
>
> Once you decide: Friday task = verify boot wiring + flip rerank metric + add hysteresis + add Telegram. ~200 LoC."

---

## 🎤 Your call dude

| Pick | Means |
|---|---|
| **A** | Accept all 4 of my recommendations (volume_desc + ATM bootstrap + 60s+hysteresis + ATM tie-break, open cohort) |
| **B** | Same as A but force depth-200 split (3 NIFTY + 2 BANKNIFTY) |
| **C** | Different — answer Q1-Q7 individually |
| **D** | Show me how the current code actually flows (deep-dive) |

Once decided I'll persist + add the Friday task. 🫡
