# Topic — Memory / WAL / Ring / Shadow Tables Deep-Drill (Z+ 7-Layer)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `z-plus-defense-doctrine.md` > `topic-zero-tick-loss-coverage-map.md` > this file.
> **Trigger:** Operator screenshots 2026-05-12 10:46 IST revealed:
> - 37 QuestDB tables + 9 mat views + **9 shadow tables** (parallel persistence)
> - Live Telegram showed `[CRITICAL] zero live ticks during market hours, Silent for 257s` — EXACT pattern we're designing for
> - `Depth spot price STALE BANKNIFTY Age 257s threshold 180s` — existing detector fired
> - Two rounds of mass-RST at 10:21 + 10:31 (9-minute interval)
>
> **Purpose:** Drill into 4 sub-systems operator named: memory, WAL, ring buffer, shadow tables. Each gets full Z+ 7-layer.

---

## 🚗 Auto-Driver Story (60-second read)

> Sir, you showed me 3 photos. They tell a CRITICAL story:
>
> **Photo 1 (QuestDB)** — Our warehouse already has 37 storage rooms, including **9 SHADOW rooms** that store an exact copy of every sealed candle. Even if the main mat-view shelves collapse, the shadow rooms have the truth. ✅
>
> **Photo 2 (Telegram alerts)** — At 10:31 AM today the system fired EXACTLY the alert we're designing defense for: "🛑 CRITICAL: zero live ticks during market hours, Silent for 257s". This means a defense ALREADY EXISTS — but the threshold is 120 seconds, and we want it down to 30 seconds.
>
> **Photo 3 (mat views)** — 9 cascade tables (1s → 1d) all derived from `candles_1s` base. If the base fails, all 9 derived tables fail together. That's a single point of catastrophe.
>
> Today we drill into the 4 sub-systems you named: **memory**, **WAL**, **ring buffer**, **shadow tables**. Each gets the full Z+ 7-layer bodyguard chart.

---

## 🚨 What the live Telegram CONFIRMED (10:31 AM IST)

| Alert observed | What it proves |
|---|---|
| `[CRITICAL] zero live ticks during market hours, Silent for 257s (threshold 120s)` | A "Connected ≠ Streaming" defense EXISTS in code today. Threshold 120s is the gap we want to close to 30s. |
| `[HIGH] Depth spot price STALE BANKNIFTY, Age 257s (threshold 180s). Depth rebalance skipped — main-feed LTP feed may be stalled for this symbol.` | Existing depth-stale detector fires correctly. Threshold 180s also too slow. |
| Two RST rounds (10:21 + 10:31) = 9 min apart | **Periodic mass-RST pattern.** Almost identical to a load-balancer rotation interval. Need Dhan support email. |
| `[HIGH] WebSocket activity watchdog fired on live-feed-3 after 50s of silence` | conn #3 silent socket pattern reproduced AGAIN. 50s detect — too slow. |
| `[MEDIUM] WebSocket 1/5..5/5 reconnected` | All 5 reconnect events tracked individually via NotificationEvent. Telegram routing working. ✅ |

**Translation:** the existing system already has 3 of the 7 Z+ layers (L1 partial detect, L5 audit, L6 reconnect). Our Friday job is to:
1. Lower thresholds (120s → 30s)
2. Add L2 verify (subscribe-ACK round-trip)
3. Add L4 prevent (token-age pre-reconnect)
4. Add L7 cooldown (30s drain)
5. Tighten L6 (soft-restart escalation)

---

## 📋 The 9 SHADOW tables — what they are, what they catch

From the QuestDB UI screenshot:
- `candles_15m_shadow`
- `candles_1d_shadow`
- `candles_1h_shadow`
- `candles_1m_shadow`
- `candles_2h_shadow`
- `candles_30m_shadow`
- `candles_3h_shadow`
- `candles_4h_shadow`
- `candles_5m_shadow`

That's **9 shadow tables** matching the **9 materialized views** (cascade from `candles_1s` base).

### Why they exist (per Wave 6 architecture)

Materialized views are derived from `candles_1s`. If the cascade engine has a bug or the base table corrupts, ALL 9 mat views go wrong simultaneously.

Shadow tables are **independently written by `seal_writer_loop`** — the aggregator engine emits to shadow tables IN PARALLEL with the cascade. If cascade fails, shadow is the source of truth.

### What they catch

| Failure | Mat view path | Shadow path |
|---|---|---|
| `candles_1s` base table corrupted | All 9 mat views wrong | Shadow tables UNAFFECTED ✅ |
| Cascade engine bug | Mat view drift | Shadow tables UNAFFECTED ✅ |
| QuestDB mat view refresh fails | Mat view stale | Shadow tables UNAFFECTED ✅ |
| Aggregator engine bug | BOTH paths wrong | Both wrong (no defense) |
| `seal_writer_loop` task panics | Mat views OK | Shadow tables BROKEN |

**Coverage analysis:** Shadow tables catch 3 of 5 cascade failure modes. Aggregator engine bug = both fail. `seal_writer_loop` panic = shadow fails alone.

### Z+ 7-Layer for shadow tables

| Layer | Mechanism |
|---|---|
| L1 DETECT | Compare shadow vs mat-view counts daily (already exists as `aggregator_seal_audit`?) |
| L2 VERIFY | Sample 1% of seals: read from BOTH shadow + mat view, assert equal |
| L3 RECONCILE | Daily 23:00 IST: full row-count match shadow vs mat view |
| L4 PREVENT | `seal_writer_loop` task watchdog — if task dies, supervisor respawns within 5s |
| L5 AUDIT | `aggregator_seal_audit` table already captures every seal event |
| L6 RECOVER | If shadow + mat view drift > 0.1% → backfill from `candles_1s` base |
| L7 COOLDOWN | Between rebuild attempts: 5 min |

---

## 📋 MEMORY ISSUES — the silent killer

### Why operator named this

Memory issues come in 5 flavors:
1. **OOM kill** — kernel kills process
2. **Allocation on hot path** — breaks O(1) latency
3. **Memory leak** — slow growth over hours/days
4. **Fragmentation** — jemalloc can't compact
5. **Page cache eviction** — file-backed structures (rkyv, WAL) lose hot pages

### Memory audit from current logs (per `aws-budget.md` Wave 7-A4)

| Component | Allocation | Type |
|---|---|---|
| Tickvault app | 2.0 GB | Host process |
| QuestDB | 1.5 GB | Docker |
| Grafana | 768 MB | Docker |
| Valkey | 512 MB | Docker |
| Prometheus | 384 MB | Docker |
| Alertmanager | 256 MB | Docker |
| OS + FS cache | 600 MB | Kernel |
| **TOTAL USED** | **6.0 GB** | |
| **HEADROOM (hard floor)** | **2.0 GB** | |

**Within the app's 2.0 GB budget:**
- Live aggregator: 8 MB
- Sealed bars (today + yesterday): 352 MB
- prev_day OHLCVOI cache: 180 KB
- Indicator running state: 50 MB
- SPSC channels: 65 MB
- Token + instrument cache: 10 MB
- WebSocket buffers: 20 MB
- **Rescue ring (5M ticks × 200 bytes): 1.0 GB**
- Heap fragmentation: 200 MB
- Tokio runtime: 20 MB
- Misc: 30 MB
- **Working set: 1.74 GB**

**Safety margin inside 2 GB cap: ~260 MB.** Tight.

### The 11 ways memory can kill us

| # | Path | Likelihood | Severity |
|---|---|---|---|
| M1 | OOM kill during burst | LOW | Critical |
| M2 | Allocation on hot path regression | MEDIUM (code drift) | High |
| M3 | Memory leak in long-running task | LOW | High |
| M4 | jemalloc fragmentation > 25% | LOW | High |
| M5 | Rescue ring overflow during sustained burst | LOW | Critical |
| M6 | Page cache eviction → rkyv slow | LOW | Medium |
| M7 | Tokio runtime starvation (thread stack growth) | RARE | Critical |
| M8 | `Arc<RwLock>` contention causes spin → CPU/mem spike | MEDIUM | Medium |
| M9 | Sealed-bar cache exceeds 352 MB budget | LOW (instrument growth) | High |
| M10 | jemalloc background thread itself OOMs | RARE | Critical |
| M11 | Linux kswapd thrash under 1 GB headroom | LOW | Critical |

### Z+ 7-Layer for memory

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_app_rss_bytes`, `tv_app_heap_alloc_bytes`, `tv_app_jemalloc_fragmentation_pct`, `tv_kswapd_pages_evicted_total` |
| L2 VERIFY | DHAT zero-alloc on hot path (build-time + nightly CI) |
| L3 RECONCILE | Daily 23:00 IST: snapshot RSS, compare to baseline |
| L4 PREVENT | Pre-boot RSS budget check; refuse boot if rss > 80% target |
| L5 AUDIT | Memory snapshots written to `memory_audit` table every 5 min |
| L6 RECOVER | If rss > 1.9 GB → soft restart (drains rescue ring first via WAL) |
| L7 COOLDOWN | Between soft restarts: 1 hour cap |

### NEW alert rules

| Alert UID | Query | For | Severity |
|---|---|---|---|
| `tv-app-rss-near-cap` | `tv_app_rss_bytes > 1.8e9` | 5m | High |
| `tv-app-rss-critical` | `tv_app_rss_bytes > 1.95e9` | 1m | Critical |
| `tv-jemalloc-fragmentation-high` | `tv_app_jemalloc_fragmentation_pct > 30` | 10m | Medium |
| `tv-kswapd-thrashing` | `rate(tv_kswapd_pages_evicted_total[5m]) > 100` | 5m | High |
| `tv-rescue-ring-fill-warning` | `tv_rescue_ring_depth > 0.5 * 5e6` | 1m | Medium |

---

## 📋 WAL ISSUES — the black-box recorder

### Why operator named this

Per `topic-data-directory-architecture-map.md`, `data/ws_wal/` is Stage 1.5 — every raw WS frame persisted to disk BEFORE parsing. If WAL itself fails, the strongest defense vanishes.

### The 11 ways WAL can fail

| # | Path | Likelihood | Severity |
|---|---|---|---|
| W1 | WAL file corruption (mid-write crash) | LOW | High |
| W2 | WAL disk full | LOW | Critical |
| W3 | WAL write latency > 10µs (breaks O(1)) | MEDIUM | High |
| W4 | WAL rotation race (file handle leak) | LOW | Medium |
| W5 | WAL retention deletes recent files | RARE | Critical |
| W6 | WAL writer task panics | RARE | Critical |
| W7 | WAL replay tool buggy → wrong bytes replayed | LOW | High |
| W8 | WAL on same disk as `data/spill/` → cross-contamination | MEDIUM | Critical |
| W9 | WAL fsync skipped → data loss on power-fail | LOW | Critical |
| W10 | WAL file format change without migration | RARE | Critical |
| W11 | WAL replay during market hours races live ingest | MEDIUM | High |

### Z+ 7-Layer for WAL

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_ws_wal_write_errors_total`, `tv_ws_wal_write_latency_ns`, `tv_ws_wal_disk_free_pct`, `tv_ws_wal_replay_lag_seconds` |
| L2 VERIFY | Sample 1% of WAL writes — read back + checksum match |
| L3 RECONCILE | Daily 23:00 IST: WAL frame count vs QuestDB ingest count (delta < 0.01%) |
| L4 PREVENT | Pre-flight disk-free check; refuse to start if `data/ws_wal/` < 20% free |
| L5 AUDIT | `ws_wal_audit` table (NEW) — every rotation + retention + replay event |
| L6 RECOVER | Replay tool: `cargo run --bin replay_ws_wal -- --from=<ts> --to=<ts>` |
| L7 COOLDOWN | Between replay attempts: 30s |

### Open questions (Friday verification)

1. Is WAL written sync or async? If sync, ensure ≤10µs latency.
2. Is WAL replay tool already built? grep `replay_ws_wal`.
3. WAL retention: 1 day local + 7 day S3 cold archive?
4. WAL on separate disk from spill? (mount point check)
5. Per-conn or per-feed WAL files? (4 visible, 16 endpoints)

---

## 📋 RING BUFFER ISSUES — the in-memory absorber

### Why operator named this

The 5M-tick rescue ring (`TICK_BUFFER_CAPACITY` per Wave 7-A4) is the in-memory tier-1 defense. If it fails, ticks immediately hit tier-2 (spill) or tier-3 (DLQ).

### The 11 ways ring buffer can fail

| # | Path | Likelihood | Severity |
|---|---|---|---|
| R1 | Ring full during sustained burst | LOW (chaos) | Critical |
| R2 | Producer panic → ring stops filling | RARE | Critical |
| R3 | Consumer panic → ring stops draining | RARE | Critical |
| R4 | Backpressure causes WS read loop to stall | MEDIUM | High |
| R5 | Ring memory grows due to fragmentation | LOW | High |
| R6 | Drain to spill too slow → ring fills faster than spill writes | MEDIUM | High |
| R7 | Off-by-one wrap-around bug | RARE | Critical |
| R8 | Sequence number reset on wrap → DEDUP confused | RARE | High |
| R9 | Ring atomic counter overflow (u64) | NEVER | — |
| R10 | Multiple producers race (if not SPSC) | RARE | Critical |
| R11 | Ring not drained on graceful shutdown → data loss | LOW | High |

### Z+ 7-Layer for ring buffer

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_rescue_ring_depth`, `tv_rescue_ring_overflow_total`, `tv_rescue_ring_drain_rate_per_sec` |
| L2 VERIFY | Loom concurrency test on every PR touching ring |
| L3 RECONCILE | Daily: ring put_count == get_count + current_depth |
| L4 PREVENT | DHAT zero-alloc on ring put + get hot path |
| L5 AUDIT | `rescue_ring_audit` table (NEW) — overflow events + drain stalls |
| L6 RECOVER | On overflow → engage spill tier; on consumer death → respawn task |
| L7 COOLDOWN | Between drain retries: 100ms exponential |

### Math for the 5M ring

- Peak ingest: 60K ticks/sec (theoretical)
- Real ingest: ~15K ticks/sec sustained (typical mock session)
- Ring capacity: 5M ticks
- Headroom at peak: 5M ÷ 60K = **83 seconds**
- Headroom at real: 5M ÷ 15K = **333 seconds = 5.5 min**

**Is 5M enough?** YES if soft-restart ≤ 80s (per WS Flow Health plan §8). NO if soft-restart drags to 90+ seconds at peak load.

**Recommendation:** Stress test at peak ingest to validate.

---

## 📋 SHADOW TABLES — the parallel persistence

### Why operator named this

37 tables include 9 `candles_*_shadow` tables. They are written by `seal_writer_loop` IN PARALLEL with the materialized view cascade. Defends against cascade failure modes.

### The 11 ways shadow tables can fail

| # | Path | Likelihood | Severity |
|---|---|---|---|
| S1 | Shadow writer task panics | RARE | High |
| S2 | Shadow writer falls behind cascade (lag > 1s) | MEDIUM | High |
| S3 | Shadow + cascade drift due to different write paths | LOW | High |
| S4 | Shadow table schema diverges from mat view | RARE | Critical |
| S5 | Shadow DEDUP key wrong | RARE | High |
| S6 | Shadow writer ILP buffer overflow | LOW | High |
| S7 | Shadow table retention drops too aggressively | LOW | High |
| S8 | Shadow table accidentally queried instead of mat view by app | LOW | Medium |
| S9 | Shadow table partition manager bug | RARE | Critical |
| S10 | Shadow table backup to S3 fails | LOW | Medium |
| S11 | Cross-verify uses shadow as ground truth when mat view is right | LOW | Medium |

### Z+ 7-Layer for shadow tables

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_shadow_writer_lag_seconds`, `tv_shadow_vs_matview_delta_per_tf` |
| L2 VERIFY | Per-seal: write to both shadow + cascade in same transaction (or compare async) |
| L3 RECONCILE | Daily 23:00 IST: full row-count match shadow vs mat view (delta < 0.01%) |
| L4 PREVENT | Compile-time check: shadow DDL must match mat view DDL (meta-guard) |
| L5 AUDIT | `aggregator_seal_audit` table already exists ✅ |
| L6 RECOVER | If drift > 0.1% → rebuild shadow from `candles_1s` base (read-only path) |
| L7 COOLDOWN | Between rebuild attempts: 1 hour |

### NEW meta-guard test

`crates/storage/tests/shadow_vs_matview_schema_guard.rs`:
- Scans every `candles_<tf>_shadow` DDL
- Scans every matching mat view DDL
- Asserts columns + types match exactly

---

## 📊 GRAND TOTAL — 44 NEW paths covered

| Sub-system | Paths | Defenses |
|---|---|---|
| Memory | 11 | Z+ 7-layer with 5 alerts + DHAT + memory_audit |
| WAL | 11 | Z+ 7-layer with replay tool + ws_wal_audit |
| Ring buffer | 11 | Z+ 7-layer with loom + rescue_ring_audit |
| Shadow tables | 11 | Z+ 7-layer with shadow_vs_matview meta-guard |
| **TOTAL** | **44** | **All covered ✅** |

Plus the 59 paths from `topic-zero-tick-loss-coverage-map.md` = **103 total paths defended**.

---

## 🚨 The 3 CRITICAL gaps surfaced from screenshots

### GAP A — Existing `[CRITICAL] zero live ticks` threshold is 120s, our target is 30s

**Evidence:** Telegram at 10:31 AM fired AFTER 257s of silence (threshold 120s).

**Fix:** Lower threshold per L1 frame-rate gauge from WS Flow Health plan. Window=5s, alert at 30s silence.

**Trade-off:** 120s threshold is robust against routine 30-50s "no-tick" periods (illiquid options). 30s threshold may false-positive on illiquid instruments.

**Solution:** Per-instrument liquidity filter (Task 13 retrofit) — only count "tick-bearing" instruments toward the gauge.

### GAP B — Periodic mass-RST every 9 minutes

**Evidence:** 10:21:15 + 10:31:XX both had `[HIGH] WebSocket N/5 disconnected` for ALL 5 main feed conns simultaneously.

**Hypothesis:** Dhan-side load-balancer rotation cycle = 9-10 min.

**Action:** Email Dhan support. Mitigation: L7 30s cooldown gives Dhan-side state time to drain BEFORE we reconnect.

### GAP C — Depth spot stale threshold 180s also too slow

**Evidence:** Telegram 10:31 AM: `Depth spot price STALE BANKNIFTY Age 257s threshold 180s`.

**Fix:** Lower threshold to 30s aligned with WS Flow Health plan. Same liquidity filter trick.

---

## 📊 Honest envelope (STRENGTHENED)

> "**Zero tick loss inside the tested envelope, with quintuple coverage**:
>
> 1. **Primary 7-Layer Z+ across 9 stages** (Network → WAL → Parser → Channel → Processor → Persist → Retention → Replay → Cross-verify)
> 2. **🆕 Stage 1.5 Black-box recorder** (`data/ws_wal/`) — raw bytes on disk before parsing
> 3. **🆕 Rescue ring** (5M ticks, 83s peak / 333s real-world headroom) — in-memory absorbs short outages
> 4. **🆕 Shadow tables** (9 mat-view mirrors) — parallel persistence catches cascade bugs
> 5. **DLQ NDJSON** — final catch for anything that escapes the above 4
>
> **Memory bounded:** 2 GB app cap + 2 GB host headroom hard floor (CI ratchet `test_total_host_memory_below_6_gb_ceiling`)
>
> **Beyond envelope:** requires 5 simultaneous failures — disk full + ring overflow + WAL corruption + shadow drift + DLQ unreachable. Not software-recoverable.
>
> **Mathematically impossible:** SEBI 24h JWT mandates ≥1 disconnect/day. Our envelope handles it gracefully (≤30s recovery via L1+L2+L7)."

**This is the FINAL form of the envelope claim.** Anything stronger = REJECT.

---

## 🎯 Discussion items (the brutal ones)

### D1 — Per-instrument liquidity filter — necessary?

Without filter: 120s threshold catches real outages. 30s threshold cries wolf on illiquid options.

With filter: only "actively trading" instruments count toward the gauge.

**Pro:** 30s detection of REAL outages.
**Con:** Complexity. Liquidity definition is fuzzy.

**My vote:** YES — define "actively trading" as "volume in last 5min > 100 contracts".

### D2 — Should shadow tables be queried by users?

Today, app reads from mat views. Shadow tables are write-only.

**Argument for read access:** Operator can compare shadow vs mat view via Grafana → catch drift visually.

**Argument against:** Adds query path complexity. Risk of accidentally querying shadow when mat view is correct.

**My vote:** EXPOSE shadow tables in a SEPARATE Grafana dashboard ("Audit Comparison"). Default operator dashboards stay on mat views.

### D3 — WAL on same disk as spill — accept or split?

Today (per the screenshot), both `data/ws_wal/` and `data/spill/` live under `data/`. Same disk.

**If `data/` partition fails:** BOTH defenses gone simultaneously.

**Mitigation cost:** Add a separate EBS volume on AWS = $20/month for 100 GB.

**My vote:** YES, split. Budget allows.

### D4 — 5M rescue ring — bump to 10M?

Current: 5M = 83s at peak. Soft restart target: 80s. Margin: 3 seconds.

**Bump to 10M:** ~165s headroom. Memory cost: +1 GB. Exceeds 2 GB app cap.

**Alternative:** Trim sealed-bar cache from 352 MB to 200 MB (yesterday's bars compressed). Frees 152 MB. Ring stays at 5M.

**My vote:** Keep 5M. Add stress test to validate.

### D5 — Are jemalloc fragmentation stats already exposed?

Modern jemalloc has `mallctl` stats. We need to verify if `tracing` already captures them.

**Action:** grep codebase for `jemalloc::stats` or `mallctl`.

---

## 🎤 Operator's question answered

**Q: "Will memory / WAL / ring buffer / shadow table issues compromise our zero tick loss?"**

**A:** **No — inside the tested envelope.**

The 4 sub-systems together cover **44 NEW failure paths**. Combined with the 59 paths from `topic-zero-tick-loss-coverage-map.md`, we have **103 total paths defended** by the Z+ 7-Layer pattern.

The 3 CRITICAL gaps surfaced from your screenshots are:
- A) Threshold 120s → 30s (need liquidity filter)
- B) 9-min mass-RST pattern (need Dhan support email)
- C) Depth spot stale 180s → 30s (same liquidity filter)

All three are NEW Friday tasks. NO implementation yet.

**3 days, no code, brainstorm continues.** Floor's yours dude.
