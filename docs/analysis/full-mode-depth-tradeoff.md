# Full-mode (5-level depth) trade-off — DISCUSSION ONLY

> **Status:** DISCUSSION / analysis. **Not** an adopted decision, **not** a plan
> item. Captured 2026-06-06 at operator request ("mark this as doc"). No code,
> no lock change. Switching stock subscriptions Quote→Full would require a dated
> operator authorization + edit to
> `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §8
> (Quote-mode lock), exactly like the universe-cap raise.
>
> **Scope of the idea:** subscribe all stock symbols (the NTM union, ~1,000 SIDs)
> in **Full mode** (Dhan response code 8, 162-byte packet, carrying **5-level
> bid/ask market depth**), while **indices stay in their current (Quote) mode**.

## 0. Critical distinction — this does NOT break the 2-WebSocket lock

| "Depth" flavor | What it is | Allowed? |
|---|---|---|
| **5-level depth inside the Full packet** (code 8, 162 B) | a richer packet on the **same** main-feed WebSocket | ✅ allowed — no new connection |
| Depth-20 (`/twentydepth`) / Depth-200 (separate WS endpoints) | dedicated extra WebSockets | ❌ forbidden forever (`websocket-connection-scope-lock.md`) |

The idea here is the **first** one. It is, however, a **mode change** (Quote→Full)
governed by the daily-universe lock §8 → needs a dated operator quote + rule edit.

## 1. Packet delta

| | Quote (today) | Full (proposed for stocks) |
|---|---|---|
| Size | **50 B** | **162 B** (3.24×) |
| Carries | LTP, volume, day OHLC, prev-close | all of Quote **+ OI + 5-level bid/ask depth** (100 B of depth) |

## 2. Storage / RAM / DB (assumptions: ~1,000 stocks, ~10 ticks/sec, 6.25 h session)

| Place | Quote today | Full mode | Verdict |
|---|---|---|---|
| RAM — latest depth snapshot | none | 5 levels × ~28 B ≈ **140 B/symbol** → ~0.25 MB total | negligible (snapshot-only) |
| RAM — 21-TF candles | OHLCV from LTP | **identical** (depth never feeds OHLC) | zero change |
| RAM — depth *history* | — | explodes if RAM-resident | avoid — write-through to DB |
| **DB (QuestDB)** | ticks + candles | **+~10,000 depth rows/sec** → ~29 GB/day raw (~6-10 GB/day compressed) | **biggest cost** — 30 GB EBS fills in days → S3 archive / shorter retention / downsample |
| Network ingress | ~0.5 MB/s | ~1.6 MB/s (+~25 GB/day) | AWS inbound = ₹0; only TLS-decrypt CPU rises a little |

**The single cost lever:** persist every depth update (full history, ~10K rows/s)
vs. downsample to the latest snapshot ~1/sec/symbol (~1K rows/s, ~10× less disk).
Option B (downsample) keeps the 30 GB EBS budget intact.

## 3. Latency — receiving vs processing (the operator's nanosecond question)

Two different latencies; do not conflate them.

| Latency | Measures | Scale | Full-mode effect |
|---|---|---|---|
| **Receiving** | bytes → kernel → TLS decrypt → WS deframe → buffer | µs–ms (network/TLS dominate) | **flat** — +112 B/packet is noise vs the µs-scale path |
| **Processing** | parse → route → dedup → candle → enqueue | ns | **+~100–200 ns/packet** (below) |

Per-packet processing breakdown:

| Stage | Quote | Full | Δ |
|---|---|---|---|
| Binary parse | ~10 ns | + 5-level depth (30 more reads + f32→f64 on 10 depth prices) | +~25–40 ns |
| Route / registry lookup / dedup | ~O(1), ~50 ns lookup | same | ~0 |
| **LTP → 21-TF candle** (trading-critical) | O(1)/TF | **identical** (same LTP) | **~0** |
| NEW depth extract + store | — | build snapshot, update slot, enqueue depth row | +~50–150 ns (if added) |

**Net:** ~**+100–200 ns per Full packet**, against a **10 µs** `full_tick_processing`
budget (~2 % of headroom). O(1) is preserved — Full mode raises the **constant**,
not the complexity class. The trading-critical LTP→candle path is **unchanged**.

### The real latency risk is NOT per-packet ns

| Risk | Why | Required discipline |
|---|---|---|
| Aggregate throughput | 3.24× bytes/sec + ~10K depth rows/sec | ~1–3 % of one core on 2-vCPU m8g.large → **no saturation**, no queue buildup |
| Allocation on depth path | a `Vec` per packet = allocator latency = tail spikes | depth must be **stack-only** (`ArrayVec<_, 5>`, zero heap; DHAT-tested) |
| Depth persistence leaking onto the LTP path | 10K ILP rows/s could backpressure | depth on a **separate** bounded channel (ring→spill→DLQ), never blocking LTP→candle |

## 4. Other places affected

- **Parser/dispatcher:** must route **per-symbol mode** (indices = Quote code 4,
  stocks = Full code 8). The dispatcher already supports both codes.
- **prev-close routing** (Ticket #5525125): NSE_EQ Full prev-close = bytes 50–53 →
  Full mode *improves* stock prev-close fidelity.
- **New surface:** depth persistence module + table, DEDUP `(security_id, segment, ts)`,
  depth metrics, f32→f64 cleaning on depth prices, possibly new error codes.
- **Locks:** no new WebSocket (2-WS lock safe); Quote→Full is a §8 mode change →
  dated operator authorization required before any code.

## 5. Bottom line (no illusion)

- **Receiving latency:** effectively flat (extra bytes are noise vs µs network).
- **Processing latency:** +~100–200 ns/Full packet; LTP→candle trading path
  unchanged; ~2 % of the 10 µs budget; O(1) intact (bigger constant).
- **The danger is never the per-packet ns** — it is zero-alloc depth, a separate
  depth channel off the hot path, and not saturating the single hot thread (it
  won't at ~10K pkts/s on 2 vCPU).
- **DB disk** is the only material cost; downsampling depth to ~1/sec/symbol keeps
  the 30 GB EBS budget.
- All figures are **engineering estimates** anchored to packet sizes + the
  `quality/benchmark-budgets.toml` budgets; if ever adopted, `cargo bench` + DHAT
  + the 5 %-regression gate would **measure** the real delta and fail the build on
  a budget breach — provable, not promised.
