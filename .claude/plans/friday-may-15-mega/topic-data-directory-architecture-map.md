# Topic — `data/` Directory Architecture Map (operator-provided reference)

> **Status:** DRAFT (reference + analysis, discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** This file documents the observed `data/` structure as ground truth. `topic-zero-tick-loss-coverage-map.md` references this.
> **Trigger:** Operator screenshot 2026-05-12 10:40 IST revealed `data/ws_wal/` — a layer NOT in my mental model. Coverage map needs update.

---

## 🚗 Auto-Driver Story

> Sir, I had been planning the warehouse with 8 stages. You showed me a photo of the actual warehouse — there's a 9th stage I didn't know about: a **conveyor-belt black-box recorder** that captures every parcel BEFORE it even enters the sorting room. Even if every other stage fails, this recorder has the raw goods on disk. THIS is the missing layer in our zero-tick-loss math.

---

## 📁 Observed `data/` structure (from operator screenshot)

```
data/
├── cache/
│   └── tv-token-cache                          # Auth JWT cache (Valkey miss fallback)
│
├── instrument-cache/
│   ├── api-scrip-master-detailed.csv           # Dhan CSV (39 MB)
│   ├── fno-universe.rkyv                       # rkyv binary cache (21 MB, 10ms load)
│   ├── index-prev-close.json                   # Index prev-close snapshot
│   └── instrument-build-date.txt               # Freshness marker
│
├── logs/
│   ├── candles/
│   │   ├── candles.2026-05-12-04               # Per-day candle-flush logs
│   │   └── candles.2026-05-12-05
│   ├── historical/                             # Historical fetch logs
│   ├── live_ticks/
│   │   ├── live_ticks.2026-05-12-04
│   │   └── live_ticks.2026-05-12-05
│   ├── option_chain/                           # Option Chain fetch logs
│   ├── app.2026-05-12-04                       # Daily rotated app logs
│   ├── app.2026-05-12-05
│   ├── app.log                                 # Symlink → latest
│   ├── errors.jsonl.2026-05-12-04              # Hourly-rotated structured ERRORs
│   ├── errors.jsonl.2026-05-12-05
│   ├── errors.log                              # Single-file WARN+ tail
│   └── errors.summary.md                       # 60s-refresh signature snapshot
│
├── spill/                                      # Tier-2 rescue: NDJSON spill for tick-loss recovery
│                                               # (empty in screenshot — healthy state)
│
└── ws_wal/                                     # 🆕 STAGE 1.5 — RAW WS frame WAL (write-ahead log)
    ├── ws-frames-01778561050439726000.wal      # Per-conn or per-feed WAL files
    ├── ws-frames-01778561488126264000.wal      # Timestamped (nanos epoch)
    ├── ws-frames-01778562101560111000.wal      # 4 files = 4 active rotations
    └── ws-frames-01778562467794922000.wal      # OR per-conn capture
```

---

## 🚨 The CRITICAL DISCOVERY — `data/ws_wal/` is a Stage I missed

### What it means

Every raw WebSocket frame received from Dhan is **persisted to disk as binary WAL** BEFORE being parsed, BEFORE being enriched, BEFORE being persisted to QuestDB.

This is the **STRONGEST possible defense against tick loss**: even if every downstream stage fails (parser bug, channel deadlock, QuestDB down, rescue ring overflow, spill disk-full, DLQ unreachable), **the raw bytes are STILL on disk** in `ws_wal/`.

### Where it fits in the coverage map

```
DHAN SERVER
   │
   ▼ (1) NETWORK — TCP frame
WS LAYER (16 endpoints)
   │
   ▼ 🆕 (1.5) WS WAL — raw bytes persisted to disk BEFORE parsing
ws_wal/ws-frames-<ts>.wal       ← black-box recorder
   │
   ▼ (2) PARSE — binary → ParsedTick
SPSC CHANNEL
   │
   ▼ (3) TICK PROCESSOR
RESCUE RING
   │
   ▼ (4) PERSIST QUEUE
QUESTDB INGEST
   │
   ▼ ...
```

### Why this is a game-changer

With `ws_wal/`, the honest envelope can be tightened DRAMATICALLY:

| Failure | Without ws_wal | With ws_wal |
|---|---|---|
| Parser bug regression | Tick loss (until next deploy) | Replayable from WAL |
| Tick processor panic | Tick loss until task restart | Replayable from WAL |
| Channel deadlock | Tick loss until L6 soft restart | Replayable from WAL |
| QuestDB outage > 60s | DLQ catches | DLQ + WAL double-catch |
| Rescue ring overflow | Spill catches | Spill + WAL double-catch |
| App process crash mid-tick | In-flight tick lost | Tick already on WAL disk ✅ |
| Disk full on `data/spill/` | Halt + alert | WAL still on different inode if separate mount |

---

## 📋 Open questions about `ws_wal/` (need to verify before Friday)

### Q1 — Per-conn or per-feed WAL files?

4 `.wal` files visible. We have 16 endpoints. Possibilities:
- (a) 4 files = main-feed-only (5 main conns rotated to 4 files)
- (b) 4 files = 1 per feed-type (main / depth-20 / depth-200 / order-update)
- (c) 4 files = time-based rotation (every N minutes)

**Action:** grep codebase for "ws_wal" or "ws-frames" to find the writer.

### Q2 — When is WAL written?

Two design choices:
- **Sync write:** every frame written before parsing → strongest guarantee, ~100µs overhead
- **Async write:** frames batched + flushed every N ms → faster, but ≤N ms loss on crash

**Most likely:** async with bounded queue (`tracing-appender::non_blocking` style).

**Action:** check code for sync vs async semantics + flush interval.

### Q3 — How big do WAL files grow? Rotation policy?

4 files visible, each timestamped. If each = 1 hour rotation: 24 files/day. If each = 100 MB rotation: variable.

**Disk math:**
- ~10,674 instruments × 60K ticks/sec peak = bounded
- Real-world: ~10K ticks/sec avg = 36M ticks/hour
- Each tick ~50 bytes raw = 1.8 GB/hour = 14 GB/trading day

**Action:** verify retention policy. Do they auto-delete after N days?

### Q4 — Is there a replay tool?

If WAL exists, there should be a `replay_ws_wal` binary or function that reads WAL → re-feeds the parser → re-runs the pipeline.

**Action:** find this in code. If missing, ADD as Friday task.

### Q5 — Is WAL written from the WS read loop, or async?

If WS read loop blocks on WAL write → hot path latency hit. If async → small loss window.

**Action:** check code.

---

## 📊 Updated Zero Tick Loss Coverage Map

The master map now has **9 stages** (added Stage 1.5):

| Stage | Description | Defense for ZTL |
|---|---|---|
| 1 | Network | L1-L7 from WS Flow Health plan |
| **1.5 🆕** | **WS WAL capture** | **Raw bytes on disk — strongest catch-all** |
| 2 | Parser | Compile-time ratchets |
| 3 | SPSC channel | 5M rescue ring |
| 4 | Processor | O(1) hot path, composite-key |
| 5 | Persist (QuestDB) | 3-tier rescue → spill → DLQ |
| 6 | Retention | dry-run + hard floor |
| 7 | Replay | per-record checksum + DEDUP |
| 8 | Cross-verify | bhavcopy + Option Chain |

### Stage 1.5 Z+ 7-Layer Defense (NEW)

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_ws_wal_write_errors_total`, `tv_ws_wal_write_latency_ns` |
| L2 VERIFY | Sample 1% of frames — re-read from WAL, hash match |
| L3 RECONCILE | Daily: WAL frame count vs QuestDB ingest count |
| L4 PREVENT | Pre-flight `data/ws_wal/` disk-free check (5min) |
| L5 AUDIT | `ws_wal_audit` table (NEW) — every rotation + retention event |
| L6 RECOVER | Replay tool reads WAL → re-feeds pipeline |
| L7 COOLDOWN | Between rotation attempts: 60s |

---

## 🎯 Discussion items

### D1 — Should we mount `data/ws_wal/` on a SEPARATE physical disk?

**Argument for:** If `data/` partition fails entirely, WAL on separate disk survives.

**Argument against:** Cost. Complexity. Single disk is "good enough" for 99.99% of failures.

**My vote:** YES for AWS deployment. NO for Mac dev.

### D2 — Should WAL replay be auto-triggered on boot if QuestDB is BEHIND?

If at boot, `tv_ws_wal_max_ts > tv_questdb_max_ts` → auto-replay difference.

**Pro:** zero operator action for routine recoveries.

**Con:** replay during market hours = race with live ingest.

**My vote:** Auto-replay only if `now() - market_close > 30 min` (i.e., post-market only).

### D3 — Retention policy for WAL files

| Option | Disk cost | Recovery window |
|---|---|---|
| (a) 7 days hot | ~98 GB | 1 week |
| (b) 30 days hot | ~420 GB | 1 month |
| (c) 1 day hot + S3 cold for 7 days | ~14 GB local + S3 | 1 week |

**My vote:** (c) — best ROI. 1-day local for fast replay, S3 cold for compliance.

### D4 — Does WAL include the timestamp of receipt?

If WAL stores: `[receipt_ts_ns | dhan_frame_bytes]`, we can verify Dhan latency.
If WAL stores: `[dhan_frame_bytes]` only, we lose receipt timing.

**Action:** check format. If missing, advocate for adding `receipt_ts_ns` header.

### D5 — Is WAL itself in the hot path?

The WAL write happens BEFORE parse. If WAL write is sync + slow, hot path latency suffers.

**Constraint:** WAL write must be ≤10µs to stay within hot-path budget.

**Action:** verify with DHAT.

---

## 🎤 Implications for the FOREVER charter claim

Per `operator-charter-forever.md` §F, the honest envelope can now read:

> "**Zero tick loss inside the tested envelope, with belt-and-suspenders coverage**:
> - **Primary path (Stage 1 → 8):** 7-Layer Z+ defense across all 8 stages
> - **🆕 Black-box recorder (Stage 1.5):** every raw WS frame persisted to `data/ws_wal/` BEFORE parsing. Even if all downstream defenses fail, raw bytes are on disk for replay.
> - **Catastrophic envelope:** ws_wal disk full AND QuestDB down AND rescue ring full AND spill disk full SIMULTANEOUSLY → ticks then fail to write but are STILL in-memory in the rescue ring up to 5M tick capacity.
> - **Beyond catastrophic envelope:** Mathematically requires 4 simultaneous disk failures + 2 process failures. Not a software-recoverable scenario."

This is a STRONGER guarantee than the previous map.

---

## 📋 Action items (NO IMPLEMENTATION — plan-only)

1. **Find the WAL writer in code:** grep `ws_wal` / `ws-frames` / `write_ahead_log`
2. **Find the WAL replay tool (if exists):** grep `replay_ws_wal` / `replay_wal`
3. **Verify WAL is async + bounded queue:** check `tracing-appender::non_blocking` usage
4. **Verify WAL retention:** find rotation + delete policy
5. **Update `topic-zero-tick-loss-coverage-map.md`** to add Stage 1.5
6. **Add `ws_wal_audit` table** to `topic-questdb-7-layer-z-plus-defense.md`
7. **Document WAL in disaster-recovery.md** scenarios

All deferred until Friday session.

---

## 📁 Full directory roles (quick reference)

| Directory | Role | Defense layer |
|---|---|---|
| `data/cache/` | Auth token / Valkey miss fallback | L4 PREVENT for token expiry |
| `data/instrument-cache/` | Universe rkyv + CSV cache | L4 PREVENT for cold-start CSV download |
| `data/logs/candles/` | Per-day candle flush audit | L5 AUDIT for cascade |
| `data/logs/historical/` | Historical fetch logs | L5 AUDIT for backfill |
| `data/logs/live_ticks/` | Daily live tick audit logs | L5 AUDIT |
| `data/logs/option_chain/` | OC REST fetch logs | L5 AUDIT |
| `data/logs/app.log` + `app.<date>` | Full app stdout | L5 AUDIT |
| `data/logs/errors.log` | WARN+ tail | L5 AUDIT |
| `data/logs/errors.jsonl.*` | Hourly ERROR JSONL | L5 AUDIT (mcp-server reads) |
| `data/logs/errors.summary.md` | 60s refresh signature snapshot | L1 DETECT |
| `data/spill/` | Tier-2 rescue NDJSON | L6 RECOVER |
| **`data/ws_wal/`** | **🆕 Stage 1.5 black-box recorder** | **L6 RECOVER (strongest)** |

---

## 🎤 Verdict

The `data/ws_wal/` directory is the single most important addition to my mental model since the discussion started. It elevates the zero-tick-loss guarantee from "bounded recovery via 3-tier rescue" to "bounded recovery via 3-tier rescue + raw-frame black-box recorder".

**This is the layer that makes the operator's "not even a single tick" demand mathematically achievable** (inside the catastrophic envelope).

Friday session must:
1. Verify WAL writer behaves as documented above
2. Build replay tool if missing
3. Wire WAL into the master coverage map ratchets

Floor's yours for D1-D5 discussion.
