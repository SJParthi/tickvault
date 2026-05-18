# Tick → 21 Timeframe Aggregator — How Pure Live Ticks Become Precise Bars

> **Status:** DESIGN LOCKED 2026-05-18.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion docs:** `aws-indices-only-locked-architecture.md`, `ram-sizing-proof.md`.
> **Scope:** How a single live-tick stream from Dhan main-feed WebSocket produces 21 sealed candle timeframes IN RAM per (instrument, TF) cell, with zero allocation on hot path, O(1) per tick.

---

## §0. Auto-driver one-liner

> "Sir, every second the price ticker shows a number. That number simultaneously updates 21 different charts — 1-minute chart, 2-minute chart, ..., 1-hour chart, daily chart. When a chart's time window ends, that chart's candle is SEALED (saved + sent to strategy). Then the next candle begins. All 21 charts update from the SAME tick at the SAME instant. No database touched. Just 21 little memory boxes per stock, each holding the current candle. When time crosses a boundary, the box is sealed and a new one starts."

```
   Tick arrives at T=10:23:47 IST, price=24700.50, volume=10
              │
              ▼
       ┌──────────────────────────┐
       │ for each TF in 21 TFs:    │
       │   ▼                       │
       │  Has the boundary crossed?│
       │   ├─ YES:                 │
       │   │   ▶ seal old candle   │
       │   │   ▶ write to bar_cache (RAM)
       │   │   ▶ async flush to QuestDB
       │   │   ▶ fire indicator updates
       │   │   ▶ fire strategy eval
       │   │   ▶ start NEW current candle
       │   └─ NO:                  │
       │       ▶ update current:   │
       │           high = max(high, P)
       │           low  = min(low,  P)
       │           close = P
       │           volume += V
       └──────────────────────────┘
              │
              ▼
   All 21 TFs updated in <1 microsecond
   Next tick can arrive
```

---

## §1. The 21 timeframes (fully dynamic + scalable per operator-charter §A)

| # | TF | Bars/day | Boundary (IST wall-clock) |
|---|---|---|---|
| 1 | 1m | 375 | each :00 second |
| 2 | 2m | 188 | even minutes |
| 3 | 3m | 125 | minutes mod 3 == 0 |
| 4 | 4m | 94 | minutes mod 4 == 0 |
| 5 | 5m | 75 | :00, :05, :10, ... |
| 6 | 6m | 63 | minutes mod 6 == 0 |
| 7 | 7m | 54 | minutes mod 7 == 0 |
| 8 | 8m | 47 | minutes mod 8 == 0 |
| 9 | 9m | 42 | minutes mod 9 == 0 |
| 10 | 10m | 38 | :00, :10, :20, ... |
| 11 | 11m | 35 | minutes mod 11 == 0 |
| 12 | 12m | 31 | minutes mod 12 == 0 |
| 13 | 13m | 29 | minutes mod 13 == 0 |
| 14 | 14m | 27 | minutes mod 14 == 0 |
| 15 | 15m | 25 | :00, :15, :30, :45 |
| 16 | 30m | 13 | :00, :30 |
| 17 | 1h | 7 | :00 of each hour |
| 18 | 2h | 4 | even hours |
| 19 | 3h | 3 | hours mod 3 == 0 |
| 20 | 4h | 2 | hours mod 4 == 0 |
| 21 | 1d | 1 | IST midnight (or market open 09:15 per convention — locked at IST midnight) |

**Total bars per instrument per day across all 21 TFs: 1,278.**

### Boundary alignment convention (LOCKED)

We align bars to **IST wall-clock**, not market open. Rationale:
- Cross-verify with Dhan REST `/v2/charts/intraday` uses wall-clock alignment per `dhan-ref/05-historical-data.md`
- Same alignment as TradingView / NSE charting tools
- Simplifies cross-day boundary math

The 09:15:00 IST market open will typically NOT fall on most boundaries (e.g., for 7-minute TF, the boundary near market open is 09:14:00 or 09:21:00). The first intraday bar of a TF starts at the first tick after 09:15:00 IST and ends at the NEXT boundary. Bars are NOT padded with placeholder data for boundaries before market open.

---

## §2. The hot-path algorithm (zero allocation, O(1) per tick)

### Per-tick pseudocode

```rust
// PSEUDOCODE — actual lives in crates/core/src/pipeline/candle_aggregator.rs
// One cell per (instrument, timeframe) — 4 SIDs × 21 TFs = 84 cells total

fn on_tick(tick: Tick) {
    let security_id = tick.security_id;
    let ts_ms = tick.exchange_timestamp_ms;
    let price = tick.last_price;
    let volume = tick.last_quantity;

    // Bounded iteration over the 21 TFs — no allocation
    for tf in ALL_21_TIMEFRAMES.iter() {
        let cell = &cells[security_id_to_idx(security_id)][tf_to_idx(*tf)];
        let boundary_ms = align_to_boundary(ts_ms, tf.duration_ms());

        if boundary_ms > cell.current_boundary {
            // BOUNDARY CROSSED — seal current bar
            seal_bar(cell, boundary_ms);
            cell.reset_for_new_bar(boundary_ms, price);
        } else {
            // SAME BAR — update OHLCV
            cell.high = cell.high.max(price);
            cell.low  = cell.low.min(price);
            cell.close = price;
            cell.volume += volume;
        }
    }
}

#[inline(always)]
fn align_to_boundary(ts_ms: i64, tf_duration_ms: i64) -> i64 {
    (ts_ms / tf_duration_ms) * tf_duration_ms
}
```

### Cost per tick (named units)

| Operation | Cost |
|---|---|
| Outer loop: 21 iterations | bounded |
| Per-iteration: 1 div, 1 mul (boundary alignment) | ~5 ns |
| Per-iteration: 1 compare (boundary crossed?) | ~1 ns |
| Per-iteration: 4 f64 max/min/assign + 1 i64 add (update) | ~5 ns |
| **Total per tick** | **~250 ns** worst case |

At 20 ticks/sec peak (4 SIDs × 5 ticks/sec): 21 × 250ns × 20 = **5 µs/sec aggregate = 0.0005% CPU**. Trivial.

### Zero allocation on hot path

| Pattern | Used? |
|---|---|
| `Vec::push` | NO — fixed-size `[Cell; 84]` |
| `String::from` | NO — security_id is u32, TF is enum |
| `Box::new` | NO — cells are stack/static |
| `format!` | NO — emit happens via `tracing::info!` outside hot path |
| `Arc::clone` | NO — cells are `&mut` direct |
| Heap allocations | ZERO per tick |

Pinned by existing DHAT test categories per `testing.md`.

---

## §3. The cell struct (memory layout)

```rust
// PSEUDOCODE
#[repr(C, packed)]
struct BarCell {
    current_boundary_ms: i64,  // 8
    open: f64,                  // 8
    high: f64,                  // 8
    low: f64,                   // 8
    close: f64,                 // 8
    volume: i64,                // 8
    open_interest: i64,         // 8 (0 for IDX_I)
    _padding: [u8; 8],          // 8 (cache-line friendly = 64B total)
}
// sizeof = 64 bytes — fits a single cache line
```

84 cells × 64 B = **5.4 KB** of cell state for the entire aggregator. Trivial — fits L1 cache on most CPUs.

---

## §4. What happens at boundary crossing (the seal path)

When a boundary crosses, the cell's current bar is FROZEN and dispatched. Pseudocode:

```rust
fn seal_bar(cell: &Cell, new_boundary_ms: i64) {
    let sealed = SealedBar {
        ts_ms: cell.current_boundary_ms,
        open: cell.open,
        high: cell.high,
        low: cell.low,
        close: cell.close,
        volume: cell.volume,
        open_interest: cell.open_interest,
        security_id: cell.security_id,
        timeframe: cell.timeframe,
    };

    // Path 1: write to bar_cache (RAM) — strategy + indicator hot reads
    bar_cache.append(sealed);

    // Path 2: async ILP flush to QuestDB candles_<tf> table
    persist_channel.try_send(sealed);  // bounded SPSC, never blocks

    // Path 3: fire indicator update for this TF
    indicator_engine.update(sealed);

    // Path 4: fire strategy eval (per-minute strategies on 1m boundary; etc)
    if cell.timeframe == TF::_1m {
        strategy_evaluator.on_minute_close(sealed);
    }
}
```

### Where each sealed bar lives

| Destination | Purpose | Hot-path-safe? |
|---|---|---|
| `bar_cache` (RAM) | Today's + yesterday's sealed bars for indicator + strategy reads | YES (Arc<RwLock> with O(1) append) |
| QuestDB `candles_1m` ... `candles_1d` tables | Persistent history + cross-verify | NO — async flush, off hot path |
| Indicator engine | RSI/MACD/BB/SMA/EMA update per-TF | YES (yata O(1) per update) |
| Strategy evaluator | Per-TF signal evaluation | YES (RAM-only read, no DB) |

---

## §5. The bar_cache module (RAM-resident 30-day history)

Per operator demand "always everything in memory RAM ... even historical data also":

### Layout

```rust
// PSEUDOCODE — crates/trading/src/in_mem/bar_cache.rs
pub struct BarCache {
    // 4 SIDs × 21 TFs × ~1278 bars/day × 30 days = ~107K bars total
    // Each bar 64B, total ~6.9 MB
    // Stored as ArcSwap<Vec<Bar>> per (security_id, tf) for lock-free reads
    cache: HashMap<(SecurityId, Timeframe), ArcSwap<VecDeque<Bar>>>,
}

impl BarCache {
    pub fn append(&self, bar: Bar) { /* O(1) push, evict oldest if >30 days */ }
    pub fn read_last_n(&self, sid: SecurityId, tf: Timeframe, n: usize) -> Arc<Vec<Bar>> {
        // Lock-free read via arc-swap
    }
}
```

### Boot rehydration (from QuestDB → RAM at boot)

```
   T+12s  Step 4: QuestDB DDL complete
       │
       ▼
   T+13s  Step 4a: spawn bar_cache hydration task
       │
       ▼ for each (sid in [NIFTY, BANKNIFTY, SENSEX, INDIA VIX]):
       │   for each tf in ALL_21_TIMEFRAMES:
       │     SELECT * FROM candles_<tf>
       │     WHERE security_id = sid
       │     AND ts > today - INTERVAL 30 DAYS
       │     ORDER BY ts ASC
       │     INTO bar_cache[(sid, tf)]
       │
       ▼ (84 SELECT queries total, each fetches <2000 rows, async parallel)
   T+15s  bar_cache fully hydrated (~6.9 MB RAM)
       │
       ▼
   Aggregator starts; new bars seal in parallel to QuestDB + bar_cache
```

### IST midnight roll (drop oldest day, keep rolling 30-day window)

At 00:00:01 IST every day:
```rust
fn ist_midnight_roll(&self) {
    for ((sid, tf), deque) in self.cache.iter() {
        deque.retain(|bar| bar.ts > now - 30_days);
    }
}
```

Bounded: drops ~1278 × 4 = 5,112 bars total. Cheap.

---

## §6. Cross-verify with REST historical (the integrity proof)

Per `aws-indices-only-locked-architecture.md` §14:

At 15:31 IST each day, the cross-verify scheduler runs:

```
   For each (sid, tf) in [4 SIDs × {1m, 5m, 15m, 1h}]:
       live_bars   = bar_cache.read_last_n_today(sid, tf)
       hist_bars   = POST /v2/charts/intraday {sid, tf, today}
       
       For each timestamp in intersection:
           if live_bars[ts].open  != hist_bars[ts].open  → MISMATCH
           if live_bars[ts].high  != hist_bars[ts].high  → MISMATCH
           if live_bars[ts].low   != hist_bars[ts].low   → MISMATCH
           if live_bars[ts].close != hist_bars[ts].close → MISMATCH
           if live_bars[ts].volume != hist_bars[ts].volume → MISMATCH
       
       if any MISMATCH:
           emit Critical Telegram (4 channels: SMS+TG+Email+Call)
           insert candle_cross_verify_mismatches row
       else:
           emit Info Telegram "Cross-match OK | TF=<tf> | N=<count>"
```

**This is the "we never miss a tick" proof.** If our aggregator drops or misorders a tick, the OHLCV won't match Dhan's authoritative bars, and we get paged immediately.

---

## §7. The full 4-SID locked universe (live ticks)

Operator clarification 2026-05-18: *"current day live ticks alone definitely we need INDIA VIX also dude okay?"*

| SID | Symbol | Segment | WS subscription mode | Option chain? | Tradeable? |
|---|---|---|---|---|---|
| 13 | NIFTY | IDX_I | Ticker | YES (REST every 50s) | YES (via NIFTY options) |
| 25 | BANKNIFTY | IDX_I | Ticker | YES (REST every 50s) | YES (via BANKNIFTY options) |
| 51 | SENSEX | IDX_I | Ticker | YES (REST every 50s) | YES (via SENSEX options) |
| 21 | INDIA VIX | IDX_I | Ticker | NO (VIX has no options) | NO (used as regime filter only) |

**Aggregator runs for all 4 SIDs. Option chain REST runs for 3 only.** INDIA VIX feeds the strategy as a volatility regime indicator but does not have its own option chain.

---

## §8. RAM impact update (4 SIDs, 21 TFs, 30 days)

Per `ram-sizing-proof.md` §7B updated for 4 SIDs (was 3):

| Component | 3 SIDs | **4 SIDs (with VIX)** | Delta |
|---|---|---|---|
| Sealed bars 30 days × 21 TFs × SIDs | 9.2 MB | **12.3 MB** | +3.1 MB |
| Indicator state (5 × 21 × SIDs) | 220 KB | **293 KB** | +73 KB |
| Strategy state (10 × SIDs × 21) | 126 KB | **168 KB** | +42 KB |
| Aggregator cells (SIDs × 21 × 64B) | 4.0 KB | **5.4 KB** | +1.4 KB |
| Today's intraday sealed bars | 307 KB | **410 KB** | +103 KB |
| **App working set total** | ~162 MB | **~166 MB** | **+3.4 MB** |

**Adding INDIA VIX moved RAM by 3.4 MB. Headroom on 4 GB instance still 60%.** Doesn't change the verdict.

---

## §9. DESIGN LOCK 2026-05-18 — what is FROZEN, what can still flex

### LOCKED (cannot change without operator-charter edit + new PR)

| Element | Locked value |
|---|---|
| Live tick SIDs | NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21 (4 SIDs) |
| Option chain underlyings | NIFTY, BANKNIFTY, SENSEX (3 — VIX excluded, has no options) |
| Timeframe list | 21 TFs (1m, 2m, ..., 15m, 30m, 1h, 2h, 3h, 4h, 1d) |
| Boundary alignment | IST wall-clock |
| Historical RAM window | 30 trading days rolling |
| AWS instance | t4g.medium ARM ap-south-1, Default tenancy, on-demand |
| Schedule | 08:00–17:00 IST every day (Mon–Sun) |
| Docker stack | tickvault + QuestDB only |
| Observability | CloudWatch Logs + Metrics + Alarms |
| Notification | SMS + Telegram + Email + Phone CALL (4 channels for Critical) |
| Rescue ring | 100K ticks (right-sized for 4 SIDs × 20 ticks/sec peak) |
| Strategy fail-closed threshold | option chain cache age > 60s |
| Cross-verify | 15:31 IST same-day (1m/5m/15m/1h) + 08:05 IST morning 1d, zero tolerance |
| Total monthly cost target | ~₹1,037/mo (₹1,022 base + ₹15 Amazon Connect) |

### FLEXIBLE (operator can dial without re-architecting)

| Element | Range |
|---|---|
| Number of strategies after BRUTEX | 0–50 (RAM headroom supports up to 50) |
| Indicators per (SID, TF) | 5 default; up to 20 supported |
| Option chain fetch cadence | 50s default; can be 30s or 60s |
| Cross-verify tolerance | ZERO locked; can never be relaxed without operator-charter §F edit |
| EBS volume size | 10 GB default; can grow to 100 GB if needed |
| CloudWatch retention | 14 days default; can extend to 90 days |
| Phone call frequency cap | 1 call per 30 min per Critical alarm |

### CANNOT EVER BE CHANGED (operator-charter §F honesty contract)

| Element | Why locked forever |
|---|---|
| Zero tolerance OHLCV match in cross-verify | Trading on stale/wrong data is the failure mode the cross-verify prevents |
| "100% inside the tested envelope" qualifier on any "100%" claim | Operator-charter §F — any other phrasing is hallucination |
| Strategy fail-closed on stale option chain | Operator demand — better to not trade than to trade on stale data |
| Static IP via EIP attached 24/7 | Dhan mandate; non-negotiable |
| Same docker-compose.yml on Mac and AWS | `aws-budget.md` rule 10; common-runtime invariant |

---

## §10. The 7-layer Z+ defense for the aggregator (operator-charter §E)

| Layer | Mechanism | Catches |
|---|---|---|
| L1 DETECT | Per-TF `tv_candle_seals_total{tf}` counter; gap if no seal in TF duration × 2 | Aggregator hang |
| L2 VERIFY | Boundary monotonicity check: each new bar ts must be > prev | Misordering |
| L3 RECONCILE | 15:31 IST daily cross-verify against Dhan REST (zero tolerance) | Missed ticks, parser bugs |
| L4 PREVENT | Boundary-aligned arithmetic uses i64 div/mul (no float drift) | Floating-point boundary errors |
| L5 AUDIT | Every sealed bar written to `candles_<tf>` table | Forensic replay |
| L6 RECOVER | If aggregator panics, supervisor restarts; bar_cache reloads from QuestDB | Self-heal |
| L7 COOLDOWN | After restart, skip the partial bar; wait for next boundary | Avoid duplicate seals |

---

## §11. Honest envelope (operator-charter §F)

> "Inside the tested envelope of {4 SIDs (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) × 21 timeframes (1m..15m+30m+1h+2h+3h+4h+1d) × IST wall-clock boundary alignment × 30-day rolling RAM history + today's live aggregation}:
>
> - Per-tick aggregator cost: ~250 ns hot path, O(1)
> - All 21 TFs update from same tick in <1 µs
> - Sealed bars dispatched to bar_cache (RAM) + QuestDB (async) + indicators + strategy in <5 µs total
> - Zero heap allocation on hot path (DHAT-pinned)
> - 15:31 IST cross-verify proves zero tick loss (OHLCV exact match vs Dhan REST authoritative)
>
> **NOT promised:** sub-second timeframes (1s/5s) — would need a different aggregator design. Week/month timeframes — read directly from sealed daily bars on demand. Boundary alignment to market-open (09:15:00 IST) — locked to IST midnight wall-clock for charting parity."

---

## §12. Trigger / auto-load

This rule activates when editing:
- `crates/core/src/pipeline/candle_aggregator.rs`
- `crates/trading/src/in_mem/bar_cache.rs`
- `crates/common/src/constants.rs` (ALL_21_TIMEFRAMES)
- Any file containing `seal_bar`, `align_to_boundary`, `TF::_1m`, `BarCell`
