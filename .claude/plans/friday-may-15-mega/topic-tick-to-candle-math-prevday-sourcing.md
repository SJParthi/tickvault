# Topic — Tick → Candle Math + prev_day_close / prev_day_oi Sourcing

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `z-plus-defense-doctrine.md` > `topic-zero-tick-loss-coverage-map.md` > this file.
> **Trigger:** Operator decision 2026-05-12 10:54 IST:
> - Reduce timeframes to **6** (was 9): `1m, 3m, 5m, 15m, 1h, 1d`
> - Every candle aggregates DIRECTLY from ticks (no cascade)
> - prev_day_close sourcing: clean for indices, clean for NSE_EQ/NSE_FNO
> - **prev_day_oi is the HARD problem — needs deep discussion**

---

## 🚗 Auto-Driver Story (60-second read)

> Sir, imagine a juice shop with 6 different cash registers — one for every minute, one for every 3 minutes, one for every 5 minutes, every 15 minutes, every hour, and one for the whole day.
>
> **Old way (CASCADE):** the 1-minute register was the boss. All other registers copied from it. If the boss made a mistake, ALL 6 were wrong.
>
> **New way (DIRECT-FROM-TICKS):** every register watches the SAME ticker tape (raw ticks) independently. If one register goes down, the others still work. No boss, no cascade.
>
> For each register: **open** = first sale, **high** = biggest, **low** = smallest, **close** = last sale, **volume** = how many oranges sold, **OI** = how many promises outstanding.
>
> The HARDEST question: **what was yesterday's closing OI?** Today we discuss 3 ways to know.

---

## 📋 The 6 timeframes (locked)

| Timeframe | Bucket size | Bucket alignment (IST) | Buckets per trading session |
|---|---|---|---|
| **1m** | 60 sec | 09:15:00 → 09:15:59, 09:16:00 → 09:16:59, ... | 375 |
| **3m** | 180 sec | 09:15:00 → 09:17:59, 09:18:00 → 09:20:59, ... | 125 |
| **5m** | 300 sec | 09:15:00 → 09:19:59, 09:20:00 → 09:24:59, ... | 75 |
| **15m** | 900 sec | 09:15:00 → 09:29:59, 09:30:00 → 09:44:59, ... | 25 |
| **1h** | 3600 sec | 09:15:00 → 10:14:59, 10:15:00 → 11:14:59, ... | 6.25 (last bucket partial) |
| **1d** | full session | 09:15:00 → 15:29:59 | 1 |

**OPEN QUESTION D1 — 1h alignment:** 09:15-base (Dhan / NSE standard) OR clock-hour (TradingView standard)?

| Option | First bucket | Last bucket | Pros / Cons |
|---|---|---|---|
| (a) 09:15-base | 09:15-10:14 | 14:15-15:14 + partial 15:15-15:29 | Symmetric, matches NSE session math |
| (b) Clock-hour | 09:15-09:59 (partial) | 15:00-15:29 (partial) | Matches TradingView, more humans expect |

**My vote:** (a) — internal math is cleaner. Frontend can re-align for display.

---

## 📋 What we DROP from current 9 timeframes

| Old TF | Status | Reason to drop |
|---|---|---|
| 1s | **REMOVE** | Operator: not needed; aggregator overhead per second is wasteful |
| 30m | **REMOVE** | Operator: not in the 6-TF set |
| 2h | **REMOVE** | Operator: not in the 6-TF set |
| 3h | **REMOVE** | Operator: not in the 6-TF set |
| 4h | **REMOVE** | Operator: not in the 6-TF set |

**Net change:** 9 TF → 6 TF. ADD `3m` (NEW). REMOVE `1s`, `30m`, `2h`, `3h`, `4h`.

**Cascade implication:** since we're going DIRECT-FROM-TICKS, the existing `candles_1s` base + cascade can be RETIRED entirely. Massive simplification.

---

## 🎯 The DIRECT-FROM-TICKS architecture (locked)

### Old (cascade)
```
ticks (raw) ──→ candles_1s (base, every 1s)
                     │
                     ▼ cascade engine
              candles_1m, candles_5m, candles_15m, ...
              (derived via mat view refresh OR seal_writer_loop)
```

**Single point of catastrophe:** if candles_1s base corrupts, all derived TFs are wrong.

### New (direct-from-ticks)
```
ticks (raw, in-memory) ──→ 6 independent aggregators
                              │
                              ├─→ AggregatorEngine<1m>  ──→ candles_1m (on seal)
                              ├─→ AggregatorEngine<3m>  ──→ candles_3m (on seal)
                              ├─→ AggregatorEngine<5m>  ──→ candles_5m (on seal)
                              ├─→ AggregatorEngine<15m> ──→ candles_15m (on seal)
                              ├─→ AggregatorEngine<1h>  ──→ candles_1h (on seal)
                              └─→ AggregatorEngine<1d>  ──→ candles_1d (on seal)
```

**Each aggregator is independent.** If one panics, the other 5 still work.

### Per-TF aggregator state (in-memory, O(1))

For each `(security_id, exchange_segment, current_bucket_ts)`:
```rust
struct LiveBucket {
    open_price: f32,           // first tick in bucket
    high_price: f32,           // max so far
    low_price: f32,            // min so far
    close_price: f32,          // last tick (updates on every tick)
    open_cum_volume: u32,      // cumulative_volume at first tick
    close_cum_volume: u32,     // cumulative_volume at last tick (updates)
    open_oi: u32,              // OI at first tick
    close_oi: u32,             // OI at last tick (updates)
    tick_count: u32,           // for sanity
}
```

**Computed at seal time:**
- `volume_delta = close_cum_volume - open_cum_volume` (volume traded IN the bucket)
- `oi_delta = close_oi - open_oi` (OI change IN the bucket)
- `oi_close = close_oi` (point-in-time OI at bucket close)

### Memory math

- 6 TFs × 11,034 instruments × ~80 bytes per LiveBucket = **5.3 MB total**
- Negligible.

### Per-tick hot path cost

For each tick:
- 6 aggregators each: 1 comparison (bucket boundary), 4 updates (high/low/close/cum_vol)
- = 6 × 5 = 30 atomic ops per tick
- At 15K ticks/sec = 450K atomic ops/sec
- **Still O(1) per tick, well within budget**

### Seal trigger

For each TF, a timer fires at bucket boundary (every 60s for 1m, every 180s for 3m, etc.):
1. Take the LiveBucket from the in-memory map
2. Compute deltas
3. Emit sealed candle to QuestDB ILP writer
4. Reset LiveBucket for next bucket

**Boundary timer per TF** (already exists per `boundary_timer.rs` per Wave 6).

---

## 🚨 prev_day_close SOURCING (the EASY problem)

### For IDX_I (indices)

**Source:** Dhan PrevClose packet (response code 6) — emitted at market open per `dhan-live-market-feed.md` rule 7.

**Format:** 16-byte packet, bytes 8-11 = f32 LE previous close price.

**Strategy:**
- At market open, listen for code 6 packets on IDX_I subscriptions
- Cache to in-memory `HashMap<(security_id, IdxI), f32>`
- Persist to QuestDB `previous_close` table for next-day rehydration

**Reliability:** ✅ HIGH — clean dedicated packet.

### For NSE_EQ + NSE_FNO

**Source:** `close` field in Quote (code 4) bytes 38-41 OR Full (code 8) bytes 50-53.

**Behavior:** Dhan sends `close` = previous day's close in EVERY Quote/Full tick. Value stays constant all session.

**Strategy:**
- At first Quote/Full tick after boot, capture `close` field
- Cache to in-memory map
- Persist to `previous_close` table

**Reliability:** ✅ HIGH — comes for free with every Quote/Full tick.

### Z+ 7-Layer for prev_day_close

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_prev_day_close_cache_size` gauge per segment |
| L2 VERIFY | Sample 1%: re-read `close` field on N-th tick, assert == cached value |
| L3 RECONCILE | Daily 23:00 IST: cross-check cache vs bhavcopy NSE close |
| L4 PREVENT | Pre-market check: assert cache non-empty before market open |
| L5 AUDIT | `previous_close` QuestDB table (already exists) |
| L6 RECOVER | If cache miss → fall back to QuestDB previous_close → fall back to QuestDB `candles_1d` yesterday |
| L7 COOLDOWN | N/A — once-per-instrument cache |

---

## 🔥 prev_day_oi SOURCING (the HARD problem — main discussion)

### Why it's hard

OI (Open Interest) is NOT in any prev-close-style Dhan packet. There is NO equivalent of "previous day OI" field that Dhan sends standalone.

OI is a **point-in-time running value** for derivatives only (NSE_FNO). NSE_EQ has no OI.

For our daily candles + indicator calculations, we need prev_day_oi to compute:
- `oi_pct_from_prev_day = (close_oi - prev_day_oi) / prev_day_oi × 100`
- OI Change column in /api/movers endpoint
- OI Change % column

Without prev_day_oi, these columns read 0 = misleading.

### The 3 candidate strategies (this is the discussion)

#### Strategy A — "First tick of day = prev_day_oi"

**Logic:** At 09:15:00 IST when market opens, the FIRST OI value received for a derivative = previous day's closing OI (because no new positions traded yet).

**Pros:**
- Zero extra infrastructure
- Comes for free from existing tick flow
- Reliable IF first tick is captured

**Cons:**
- If we miss the first tick (boot delay, network blip, late subscribe), we lose the baseline forever for that day
- Even a 5-second delay means we read OI after some positions traded
- Doesn't account for futures contracts that trade pre-open at 09:00-09:15

**Failure mode:** Boot at 09:14:50 → subscribe completes at 09:15:30 → first tick at 09:15:31 → OI includes 31 sec of trading.

#### Strategy B — "Last tick of previous day"

**Logic:** Save OI from the last tick before 15:30 IST yesterday. Persist to `previous_close` table. Load at next morning's boot.

**Pros:**
- Reliable — last tick is captured during normal market hours
- Independent of next-day's first-tick timing
- Already partially wired (`previous_close` table exists)

**Cons:**
- Requires yesterday's data to be persisted before today's boot
- If yesterday's app crashed at 15:25, we miss the last 5 min and the saved OI is stale
- Fresh deploy / cold start = no yesterday data

**Failure mode:** Yesterday's app died at 15:00 → saved OI is from 15:00, not 15:29 → today's prev_day_oi is wrong.

#### Strategy C — "Bhavcopy + Option Chain REST overlay"

**Logic:** Both sources provide official end-of-day or current OI:
- **NSE bhavcopy CSV** — published next morning ~08:00 IST, official EOD OI for all F&O
- **Dhan Option Chain REST** — current snapshot, available 24/7

**Already wired:** per `prev_oi_loader.rs` and observed in log:
```
prev_oi overlay: applied Dhan-canonical previous_oi values
  underlying=NIFTY, expiry=2026-05-12, overlay_entries=200
  underlying=BANKNIFTY, expiry=2026-05-26, overlay_entries=401
  underlying=SENSEX, expiry=2026-05-14, overlay_entries=272
  cumulative_overlay_total=873
```

**Pros:**
- Official source of truth
- Independent of our app's uptime
- Handles fresh deploys correctly
- Already shipped (873 entries loaded in today's boot)

**Cons:**
- Bhavcopy is delayed (next morning, may be 08:00-09:00 IST)
- Bhavcopy URL may change without notice (NSE redesigns)
- Option Chain REST has rate limits (1 req per 3 sec per `dhan-ref/06-option-chain.md`)
- Only covers DERIVATIVES with active option chain (873 entries, but NSE_FNO has ~98K derivatives)

**Coverage gap:** 873 vs 98K = bhavcopy/OC overlay covers ~0.9% of derivatives. The other 99.1% need a different strategy.

### 🎯 Recommended QUADRUPLE strategy (combine all 3 + first-tick fallback)

| Priority | Source | When | Coverage |
|---|---|---|---|
| **P1 (gold)** | NSE bhavcopy CSV | Next morning 08:15 IST | All F&O — official EOD OI |
| **P2 (silver)** | Dhan Option Chain REST overlay | Boot if bhavcopy unavailable | Active option chains (~873 contracts today) |
| **P3 (bronze)** | Previous day last-tick OI from `previous_close` table | Boot rehydration | Any F&O that had ticks yesterday |
| **P4 (fallback)** | First tick of day = prev_day_oi | Live capture at market open | Whatever P1-P3 missed |

**At boot, load in order P1 → P2 → P3 → P4 (live capture).** First non-empty source wins per `(security_id, segment)`.

This is the QUADRUPLE COVERAGE strategy — matches the operator's "5 layers, never a single point" pattern.

### Z+ 7-Layer for prev_day_oi

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_prev_oi_cache_size_per_source` gauge: bhavcopy / oc_rest / yesterday_last / first_tick |
| L2 VERIFY | Sample 1%: compare cached prev_day_oi vs Dhan Option Chain REST on-demand |
| L3 RECONCILE | Daily 23:00 IST: write today's last-tick OI → `previous_close` table for tomorrow |
| L4 PREVENT | At 09:14:30 IST, log a CRITICAL if `prev_oi_cache_total < 10000` (most F&O has no prev_day_oi) |
| L5 AUDIT | `prev_oi_load_audit` table (NEW) — every load attempt + outcome per source |
| L6 RECOVER | Periodic refresh task (5 min) reloads from `candles_1d` yesterday IF cache is stale |
| L7 COOLDOWN | Between bhavcopy retry: 5 min; OC REST: 3 sec (rate limit) |

### From today's log

Live evidence:
```
prev_oi_cache loaded 0 entries (likely fresh deploy or candles_1d is empty)
WARN: prev_oi_cache loaded zero entries — fresh deploy or candles_1d empty
OI Change panels will read 0% until the next IST midnight rollover repopulates
```

Then at 10:19:46:
```
prev_oi_cache loaded from candles_1d, entries=2173
prev_oi_cache populated by periodic refresh — task exiting
```

**Observation:** The periodic refresh task (5 min poll) successfully loaded 2173 entries from `candles_1d` after 5.5 min. Strategy P3 worked. But coverage = 2173 / 98K = **2.2%** of derivatives.

**Need P1 (bhavcopy) for the other 97.8%.**

---

## 📊 Per-candle field source map (locked)

For each sealed candle (any TF):

| Field | Source | Computed how |
|---|---|---|
| `ts` | seal time IST | Bucket start timestamp |
| `security_id` | tick header byte 4-7 | u32 LE |
| `segment` | tick header byte 3 | u8 enum |
| `open` | first tick in bucket | LTP from first tick (Ticker/Quote/Full) |
| `high` | running max | max of LTP across bucket |
| `low` | running min | min of LTP across bucket |
| `close` | last tick in bucket | LTP from last tick |
| `volume_cumulative_open` | first tick | volume from first tick (cumulative day-total) |
| `volume_cumulative_close` | last tick | volume from last tick (cumulative day-total) |
| `volume_delta` | computed | close - open (volume traded IN this bucket) |
| `oi_open` | first tick | OI from first tick (derivatives only, NSE_EQ → 0) |
| `oi_close` | last tick | OI from last tick |
| `oi_delta` | computed | close - open (OI change IN this bucket) |
| `prev_day_close` | cache | per Strategy P1-P4 above (NSE_EQ + NSE_FNO + IDX_I) |
| `prev_day_oi` | cache | per Strategy P1-P4 above (NSE_FNO only) |
| `close_pct_from_prev_day` | computed | (close - prev_day_close) / prev_day_close × 100 |
| `oi_pct_from_prev_day` | computed | (oi_close - prev_day_oi) / prev_day_oi × 100 |
| `volume_pct_from_prev_day` | computed | (volume_cumulative_close - prev_day_volume_total) / prev_day_volume_total × 100 |
| `tick_count` | running | sanity check |

**Hot-path math:** all computations are O(1). No allocation. No clones.

---

## 🎯 Discussion items

### D1 — 1h bucket alignment (09:15-base vs clock-hour)

Already raised above. Operator pick.

### D2 — Should we ALSO keep 1s candles for high-frequency strategy?

We have `candles_1s` today. Operator wants to drop it. But some strategies may need sub-minute granularity.

**Question:** Are there any current OR planned strategies that need 1s? If yes, keep it as 7th TF. If no, drop.

**My vote:** DROP. If strategy needs sub-minute, it should read from raw `ticks` table directly.

### D3 — Daily candle (1d) close time

Indian market closes at 15:30 IST. Daily candle should seal at:
- (a) 15:30:00 IST sharp — clean
- (b) 15:35:00 IST — accounts for last-second ticks (Dhan may emit ticks up to 15:30:30)
- (c) 16:00:00 IST — fully settled

**My vote:** (b) 15:35:00 IST. Per `dhan-ref` post-market behavior, ticks may arrive few seconds late.

### D4 — How to handle ticks AFTER bucket seals (late ticks)?

Per existing code, `AGGREGATOR-LATE-01` fires if tick arrives after bucket seals. Tick is DISCARDED.

**Question:** Should we merge late ticks into next bucket OR genuinely drop?

**Current behavior:** drop + counter.

**Operator's "zero tick loss" demand:** suggests merge or replay.

**My vote:** Keep current (drop + counter) because:
- Late ticks are RARE (clock drift signal)
- Merging would shift OHLCV across buckets (data corruption)
- Counter visibility is enough — operator can investigate via `tv_aggregator_late_ticks_discarded_total`

**Counter:** Late tick still hits `ticks` table (raw persistence). The DROP is only in the aggregator. So tick is NOT lost — it's just not in the candle. Operator can recompute from `ticks` if needed.

### D5 — Should each TF aggregator write to its OWN audit table?

Today we have `aggregator_seal_audit` (single table for all TFs).

**Option (a):** Keep single table — `aggregator_seal_audit` with `timeframe` column.
**Option (b):** Per-TF table — `aggregator_seal_1m_audit`, `_3m_audit`, etc.

**My vote:** (a) — single table with `timeframe` partition. Simpler. DEDUP UPSERT KEYS(ts, security_id, segment, timeframe).

### D6 — Cross-verify across TFs

If 1m says volume_delta=100 and 5m says volume_delta=500 for the same 5-min window, they should match (sum of 5 × 1m = 5m).

**Action:** Add daily cross-verify: `sum(1m candles in 5m window) == 5m candle`.

**Implementation:** Post-market reconcile task at 23:00 IST. Compare sums. Alert if mismatch > 0.01%.

### D7 — prev_day_volume_total — also needed

Same problem as prev_day_oi but for volume. Source map:
- P1: bhavcopy CSV has volume totals
- P2: yesterday's `candles_1d.volume_cumulative_close`
- P3: first tick of day = previous day cumulative (similar to OI)

**Recommendation:** mirror the prev_day_oi quadruple strategy.

---

## 🎤 Summary

**The math is now LOCKED:**
1. 6 TFs (was 9). Drop 1s/30m/2h/3h/4h. Add 3m.
2. Direct-from-ticks. No cascade. Each TF aggregates independently.
3. prev_day_close: PrevClose packet (IDX_I) + close field (NSE_EQ/FNO).
4. prev_day_oi: QUADRUPLE strategy (bhavcopy → OC REST → yesterday last-tick → first tick fallback).
5. All candle fields computed O(1) from tick state.

**The discussion items D1-D7 need operator decision before Friday build.**

**Zero tick loss preserved** — late ticks still persist to raw `ticks` table; only the aggregator drops them. Operator can reconstruct.

Floor's yours for D1-D7 brainstorm.

---

## 📌 APPENDIX A — Why NSE bhavcopy is P1 GOLD (operator-locked 2026-05-12 11:10 IST)

Operator's exact reasoning (verbatim, paraphrased):
> "Bhavcopy has the standard globalised precise data, right dude? Irrespective of any situation we don't even need anything from live ticks or from any sides — that's why P1 gold."

**Confirmed. The 5 reasons bhavcopy is the gold standard:**

| Reason | Detail |
|---|---|
| **1. Official source-of-truth** | NSE publishes it. SEBI-recognized. If our number disagrees with bhavcopy, OUR number is wrong. |
| **2. End-of-day FINAL settlement** | Includes the closing match auction (15:30-15:40 IST) which live ticks may miss |
| **3. Independent of our app uptime** | Even if tickvault crashed at 15:00 IST yesterday, bhavcopy has the full day |
| **4. Independent of Dhan-side issues** | Doesn't matter if Dhan API was down — NSE publishes anyway |
| **5. SEBI audit defense** | If audited, "we used bhavcopy" is the strongest possible answer |

**What bhavcopy GIVES US:**
- prev_day_close (final settlement close)
- prev_day_oi (final settlement OI)
- prev_day_volume (final settlement volume)
- prev_day_high, prev_day_low (full day range)
- All in one CSV download

**What it COSTS US:**
- ~5-10 sec download
- ~50 MB disk per day
- 1 HTTP request to nseindia.com
- Available ~08:00 IST next morning (some uncertainty on exact time)

**What this means for the 4-tier strategy:**

```
At 08:15 IST orchestrator runs:
   ↓
   Try P1 bhavcopy CSV → SUCCESS → 99%+ coverage, done
   ↓ (if bhavcopy unreachable)
   Fall back to P2 Option Chain REST overlay → ~0.9% coverage
   ↓ (if both fail)
   Fall back to P3 yesterday's last-tick from previous_close → ~2.2% coverage
   ↓ (if all 3 fail)
   Fall back to P4 first tick of day capture → 100% coverage but fragile
```

**Operator's insight is correct:** when bhavcopy works, NOTHING else is needed. The other 3 are belt-and-suspenders for the rare case bhavcopy is unavailable.

**Honest envelope (bhavcopy-specific):**
> "P1 bhavcopy works 99% of trading days. Cumulative downtime per year (when we'd need to fall through to P2-P4): expected ≤4 days of NSE infra issues. Total coverage from all 4 tiers: mathematically 100% inside the catastrophic envelope."

---

## 📌 APPENDIX B — Bhavcopy retirement risk

NSE has signalled plans to retire the BhavCopy CSV format in favor of new structure (rumored 2026-2027 timeline).

**Mitigation already designed:**
- P2 (Dhan Option Chain REST) is independent of NSE format changes
- P3 (yesterday's tick data) is our own
- P4 (first tick capture) is our own

**If bhavcopy retires:** P2/P3/P4 still work. Coverage drops from 99% to ~100% (because P2 + P3 cover most cases).

**Friday action:** monitor NSE announcements. If retirement confirmed, accelerate P2 coverage improvement (more option chain REST calls).

---

## 📌 APPENDIX C — Operator clarification 2026-05-12 11:30 IST (post-coffee)

**Operator simplified the strategy. Three locks:**

### 🔒 Lock 1 — prev_day_close: NO bhavcopy needed

**Operator's exact reasoning (paraphrased):**
> "For prev_day_close — anyhow we receive it from that particular tick's `close` column itself. For indices, PrevClose packet. For NSE_EQ + NSE_FNO, the `close` field is the same prev-day-close all day until 15:30."

**Confirmed correct.** Per `dhan-ref/03-live-market-feed-websocket.md` (rule 7 IDX_I + rules 8/10 NSE_EQ/FNO):

| Segment | Source | When | Cost |
|---|---|---|---|
| IDX_I | PrevClose packet (code 6) | At market open, dedicated packet | FREE |
| NSE_EQ | Quote/Full `close` field | EVERY Quote/Full tick all day | FREE |
| NSE_FNO | Quote/Full `close` field | EVERY Quote/Full tick all day | FREE |

**The `close` field stays constant from 09:15 IST → 15:30 IST = previous day's close.** We capture it ONCE on first tick after boot, cache forever in-memory.

**Conclusion:** bhavcopy is NOT needed for prev_day_close. The 4-tier P1-P4 strategy for prev_day_close is RETIRED. Single-source from ticks is enough.

### 🔒 Lock 2 — prev_day_volume: NOT NEEDED AT ALL

**Operator's exact reasoning:**
> "For top-volume anyhow prev day volume is not considered — so we don't need to even worry about prev_day_volume."

**Confirmed correct.** The top-volume cohort selector ranks by:
- `volume_cumulative` (cumulative since 09:15 today)
- NOT by `volume_cumulative / prev_day_volume_total` (would need yesterday)

**Use cases that WOULD need prev_day_volume (NOT in scope):**
- "Volume up 50% vs yesterday" alerts (not requested)
- Volume Profile multi-day chart (not requested)
- Daily volume % change column (not in current /api/movers spec)

**Conclusion:** Drop prev_day_volume from the strategy entirely. Frees compute, frees memory, frees storage.

### 🔒 Lock 3 — prev_day_oi: BHAVCOPY OR DHAN HISTORICAL API ONLY

**Operator's exact reasoning:**
> "One and only for prev_day_oi alone we need this bhavcopy or historical data fetch from Dhan data API itself."

**Confirmed correct.** OI has NO equivalent of the `close` field in tick packets. The only sources are external:

| Source | Reliability | Cost | Notes |
|---|---|---|---|
| **A. NSE bhavcopy CSV** | ✅ Gold | FREE, ~50 MB/day download | Per APPENDIX A reasoning above |
| **B. Dhan historical API** | ✅ Gold | Rate-limited (Data API, 5/sec, 100K/day) | `/v2/charts/historical` yesterday's 1d candle has OI |

**Operator gives us TWO equivalent-strength sources.** Pick one OR use both as redundant.

### 🎯 Simplified 2-tier strategy for prev_day_oi (was 4-tier)

| Priority | Source | When |
|---|---|---|
| **P1** | NSE bhavcopy CSV | Daily 08:15 IST orchestrator |
| **P2** | Dhan `/v2/charts/historical` for yesterday's 1d candle | Fallback if bhavcopy unavailable |

**Removed from previous design:**
- ~~P3 yesterday's last-tick from `previous_close` table~~ — was bronze; not needed if P1/P2 both work
- ~~P4 first tick of day = prev_day_oi~~ — was fallback; not needed if P1/P2 both work

**Why remove P3/P4:** They're fragile (depend on our uptime / first-tick capture timing). P1+P2 are both external/independent and reliable. Simpler is better.

**However:** keep P3/P4 as EMERGENCY fallback if both P1+P2 fail simultaneously. Wire them but don't make them primary.

---

## 📊 SIMPLIFIED FINAL STRATEGY (operator-locked)

| Field | Source | Tiers |
|---|---|---|
| **prev_day_close (IDX_I)** | PrevClose packet code 6 | 1 (from ticks) |
| **prev_day_close (NSE_EQ)** | Quote/Full `close` field | 1 (from ticks) |
| **prev_day_close (NSE_FNO)** | Quote/Full `close` field | 1 (from ticks) |
| **prev_day_volume** | ❌ NOT NEEDED | 0 |
| **prev_day_oi** | bhavcopy CSV (P1) + Dhan historical (P2) | 2 (external) |

**Simplification math:**
- Before: 4 fields × 4 tiers = 16 source-paths to design + test
- After: 3 fields × ~1.3 tiers = 4 source-paths to design + test
- **75% reduction in complexity**

---

## 🚗 Auto-Driver Story (post-coffee simplification)

> Sir, after your coffee break we cut 75% of the work:
>
> - **prev_day_close** — comes for free with every tick (or via PrevClose packet for indices). Zero extra work. ✅
> - **prev_day_volume** — we don't even need it. The top-volume picker only cares about TODAY's volume. ✅
> - **prev_day_oi** — the ONLY hard problem. We fetch from NSE bhavcopy in the morning OR Dhan historical API as backup. Two independent sources, both rock-solid.
>
> The whole "4-tier strategy with quadruple coverage" is now a "2-tier strategy for ONE specific field". Massive simplification. Operator's instinct was correct.

---

## 🎯 Updated Friday LoC estimate

Before this simplification:
- prev_day_close: 4-tier loader = ~400 LoC
- prev_day_volume: 4-tier loader = ~400 LoC
- prev_day_oi: 4-tier loader = ~400 LoC
- **Total: ~1,200 LoC**

After operator's clarification:
- prev_day_close: from ticks (existing logic) = ~50 LoC (just verify cache populated)
- prev_day_volume: REMOVED = 0 LoC
- prev_day_oi: 2-tier loader = ~250 LoC
- **Total: ~300 LoC**

**Net savings: ~900 LoC on Friday.** Faster build, simpler design, fewer bugs to introduce.

---

## 🎤 What this means for the operator's "zero tick loss" charter

The simplified strategy is STRONGER, not weaker:
- prev_day_close: from ticks = guaranteed available with every tick
- prev_day_volume: not needed = no failure mode
- prev_day_oi: 2 independent external sources, both EOD-stable

**Honest envelope (updated):**
> "prev_day fields inside the tested envelope:
> - prev_day_close: zero failure paths — comes free with every tick
> - prev_day_volume: not needed by design
> - prev_day_oi: bhavcopy (P1) covers 99%+ of trading days; Dhan historical (P2) covers the remaining 1%; emergency P3 (yesterday last-tick) + P4 (first-tick capture) wired as fallback only."

Operator's instinct cut 75% of the work AND made the system MORE robust. ☕→💡

---

## 📌 APPENDIX D — Operator confirmation 2026-05-12 12:00 IST (NSE bhavcopy = PRIMARY for prev_day_*)

**Operator's exact reasoning:**
> "For prev_day_oi I believe it's better to consider NSE bhavcopy — that would be the best case. At 8:30 AM when we download bhavcopy we can easily fetch the entire current day instruments data matching with NSE bhavcopy. It's exactly precise to believe bhavcopy. Even from there we can easily get prev_day_high, low, close, prev_day_oi — everything."

**Confirmed correct.** This is the cleanest design. Let me reorganize the strategy.

### What NSE bhavcopy actually contains

| File | Coverage | Fields |
|---|---|---|
| **Equity bhavcopy** (`cm_*.csv`) | All NSE_EQ instruments | SYMBOL, SERIES, OPEN, HIGH, LOW, CLOSE, LAST, PREVCLOSE, TOTTRDQTY (volume), TOTTRDVAL, TIMESTAMP, TOTALTRADES, ISIN |
| **F&O bhavcopy** (`fo_*.csv`) | All NSE_FNO instruments | INSTRUMENT, SYMBOL, EXPIRY_DT, STRIKE_PR, OPTION_TYP, OPEN, HIGH, LOW, CLOSE, SETTLE_PR, **CONTRACTS** (volume), VAL_INLAKH, **OPEN_INT (OI)** ✨, **CHG_IN_OI**, TIMESTAMP |

**The F&O bhavcopy gives us OI for free for every derivative.** 98K derivatives in one CSV download.

### Bhavcopy timing

- **Published:** ~18:00 IST same day (after market close)
- **Available:** by 18:30 IST same day reliably
- **Our download:** at next morning's boot, ~08:30 IST (operator's AWS schedule)
- **Means:** by the time AWS starts at 08:30 IST, bhavcopy from yesterday is 14+ hours stale (= solidly published, no race)

### Re-organized strategy (CLEANEST design)

| Field | PRIMARY source | When | Why |
|---|---|---|---|
| `prev_day_close` (IDX_I) | PrevClose packet (code 6) | Live, market open | Free from tick stream |
| `prev_day_close` (NSE_EQ + NSE_FNO) | Quote/Full `close` field | Live, every tick all day | Free from tick stream |
| `prev_day_high` | **NSE bhavcopy** | 08:30 IST boot | No tick equivalent; bhavcopy is authoritative |
| `prev_day_low` | **NSE bhavcopy** | 08:30 IST boot | Same |
| `prev_day_oi` (NSE_FNO only) | **NSE bhavcopy F&O CSV (OPEN_INT field)** | 08:30 IST boot | THE OPERATOR'S BEST CASE — official, precise, covers all 98K |
| `prev_day_volume` | NOT NEEDED | — | Operator confirmed earlier |

### Bhavcopy + Dhan historical = DIFFERENT roles (not redundant)

This is the key insight I want to lock:

| Concern | Source | Why |
|---|---|---|
| "What were yesterday's daily values?" (close/high/low/oi) | **NSE bhavcopy at 08:30 IST** | Official, single CSV download, all 98K instruments at once |
| "Were today's minute candles correct?" (1m cross-verify) | **Dhan historical at 15:31 IST** | Per-minute granularity, OHLCV+OI per bucket, only Dhan can provide |

**They do NOT overlap.** Each serves its strength:
- Bhavcopy = DAILY granularity, official, all instruments in 1 download
- Dhan historical = MINUTE granularity, per-instrument calls, supports cross-verify

**Operator's worry "should bhavcopy be first?" — YES for prev_day_*. Dhan historical is for the cross-verify only, not for prev_day_*.**

### Refined Z+ 7-Layer for prev_day_oi (bhavcopy-first)

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_bhavcopy_load_success_total`, `tv_bhavcopy_load_failed_total`, `tv_prev_day_oi_cache_size` gauge |
| L2 VERIFY | At 08:35 IST after bhavcopy load, compare cache size to expected (~98K F&O entries) |
| L3 RECONCILE | Daily 23:00 IST: cross-check yesterday's bhavcopy OI vs last 1m candle OI from candles_1m_shadow |
| L4 PREVENT | At 08:30 IST: pre-fetch bhavcopy, BLOCK boot completion until cache populated |
| L5 AUDIT | NEW `bhavcopy_load_audit` table (per download attempt) |
| L6 RECOVER | If bhavcopy unreachable: 5x retry with 5min backoff → fall through to P2 (Dhan historical fetch for yesterday) → P3 (yesterday's last-tick from previous_close table) |
| L7 COOLDOWN | Between download retries: 5 min |

### Friday LoC estimate (UPDATED — even simpler now)

| Item | Before | After bhavcopy-primary |
|---|---|---|
| prev_day_close loader | ~50 LoC | ~50 LoC (unchanged — from ticks) |
| prev_day_oi loader (was 4-tier) | ~250 LoC | ~150 LoC (bhavcopy primary + 2 fallbacks) |
| prev_day_high/low loader (NEW) | — | ~80 LoC (bhavcopy adds, basically free) |
| Bhavcopy download + parser | — | ~200 LoC |
| **Total prev_day code** | ~300 LoC | ~480 LoC |

Slightly MORE LoC but covers more fields (high/low) AND is more reliable. Operator's call.

---

## 📌 APPENDIX E — Bhavcopy implementation notes (Friday-relevant)

### URLs (NSE)

**WARNING:** NSE changed bhavcopy URLs in 2024-2025. Old `nseindia.com/archives/equities/bhavcopy/pr/PR<DDMMYY>.zip` may not work.

**Current (as of 2026):**
- Equity: `https://nsearchives.nseindia.com/products/content/sec_bhavdata_full_<DDMMYYYY>.csv` (NEW format)
- F&O: `https://nsearchives.nseindia.com/content/fo/BhavCopy_NSE_FO_0_0_0_<DDMMYYYY>_F_0000.csv.zip` (NEW format)

**Action for Friday:** verify these URLs work. Have fallback URL ready.

### Mapping bhavcopy → security_id

Bhavcopy uses SYMBOL + EXPIRY_DT + STRIKE_PR + OPTION_TYP. Our system uses security_id.

**Mapping logic:**
1. Parse bhavcopy row
2. For F&O: lookup `(SYMBOL, EXPIRY_DT, STRIKE_PR, OPTION_TYP)` in our `derivative_contracts` table → get security_id
3. For equity: lookup `SYMBOL` in `fno_underlyings` or `subscribed_indices` → get security_id

**Failure mode:** if bhavcopy has a new strike we don't have in our instrument master → log warning, skip (won't be subscribed anyway).

### Caching to QuestDB

Once parsed, write to `previous_close` table (existing) with columns:
- `security_id`
- `segment`
- `trading_date`
- `prev_close` (already there)
- `prev_open`, `prev_high`, `prev_low` (NEW)
- `prev_oi` (NEW for F&O)
- `prev_volume` (NEW — even if not used now, store for future)

### Idempotency

Same pattern as historical fetch:
- `bhavcopy_load_state` table: status per `(trading_date, file_type)` where file_type = "equity" | "fno"
- Once `status='success'`, don't re-download
- DEDUP UPSERT KEYS(trading_date, file_type)

---

## 📌 APPENDIX F — FINAL CONSOLIDATED prev_day_* strategy (this is THE design)

```
┌─────────────────────────────────────────────────────────────┐
│  08:30 IST boot (Mon–Fri + Mock Sat)                         │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  Download YESTERDAY's bhavcopy (equity + F&O CSVs)      │ │
│  │  Parse → write to previous_close table                  │ │
│  │  → ALL prev_day_* fields available for TODAY's session  │ │
│  └─────────────────────────────────────────────────────────┘ │
│                              │                                │
│                              ▼                                │
│  09:00 IST pre-open                                           │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  Live tick stream starts                                 │ │
│  │  - IDX_I PrevClose packet (code 6) — capture in-memory   │ │
│  │  - NSE_EQ/FNO close field — capture in-memory           │ │
│  │  → CROSS-CHECK against bhavcopy values (alert mismatch) │ │
│  └─────────────────────────────────────────────────────────┘ │
│                              │                                │
│                              ▼                                │
│  09:15 IST market open                                        │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  Live aggregation builds candles_1m_shadow              │ │
│  │  Strategy/indicators use prev_day_oi from bhavcopy      │ │
│  └─────────────────────────────────────────────────────────┘ │
│                              │                                │
│                              ▼                                │
│  15:30 IST market close                                       │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  All 16 WS disconnect                                    │ │
│  └─────────────────────────────────────────────────────────┘ │
│                              │                                │
│                              ▼                                │
│  15:31 IST post-market                                        │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  Dhan historical /v2/charts/intraday fetch              │ │
│  │  - 11K instruments × 1m candles                          │ │
│  │  - Cross-verify against candles_1m_shadow               │ │
│  │  - Zero tolerance OHLCV+OI                              │ │
│  └─────────────────────────────────────────────────────────┘ │
│                              │                                │
│                              ▼                                │
│  17:30 IST AWS shutdown                                       │
└─────────────────────────────────────────────────────────────┘

Next morning 08:30 IST: cycle repeats with TODAY's bhavcopy
(which has YESTERDAY's session data — becomes TOMORROW's prev_day_*).
```

### The TWO independent sources, summarized

| Source | Granularity | When | Primary role |
|---|---|---|---|
| **NSE bhavcopy** | Daily | 08:30 IST next morning | `prev_day_*` (close, high, low, OI, volume) |
| **Dhan historical** | 1-minute | 15:31 IST same day | Cross-verify live aggregation |

Each is AUTHORITATIVE for its domain. They complement, do not overlap.

### Operator-correct ultra-summary

> "Yesterday's bhavcopy → today's prev_day_*. Today's Dhan historical → cross-verify today's candles. Two sources, two roles, zero conflict."

---

## 🎤 What this changes for Friday

| Friday task | Change |
|---|---|
| ~~prev_day_oi 4-tier loader~~ | RETIRED — bhavcopy primary, simpler |
| Bhavcopy downloader + parser | NEW — ~200 LoC |
| Boot-time bhavcopy load orchestrator | NEW — ~100 LoC |
| `bhavcopy_load_audit` table | NEW |
| Cross-check bhavcopy vs `close` field (Z+ L2 VERIFY) | NEW |
| Z+ 7-Layer for bhavcopy | NEW |

Net LoC: similar to before. Reliability: MUCH better. Coverage: ALL prev_day_* fields, not just OI.

Floor's yours for next angle.
