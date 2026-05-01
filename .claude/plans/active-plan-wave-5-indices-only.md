# Implementation Plan: Wave 5 — Indices-Only Subscription + Depth Redesign + Resilience Hardening

**Status:** APPROVED
**Date:** 2026-05-01
**Approved by:** Parthiban (operator) — verbatim "yes approved" 2026-05-01
**Branch:** `claude/fetch-status-info-V56GN`
**Triggering context:** Operator decision 2026-05-01: drop the 216-stock F&O subscription, focus exclusively on NIFTY/BANKNIFTY/SENSEX indices (all expiries, all strikes), redesign depth-20 + depth-200 around major-index single-side connections + dynamic top-volume gainers, wire core_affinity, fix two hot-path bugs caught by adversarial review.

## Honest Charter (per `wave-4-shared-preamble.md` §2 + §8)

> "100% inside the tested envelope, with ratcheted regression coverage: ≤60s QuestDB outage absorbed by rescue→spill→DLQ; ≤600K rescue ring capacity; bench-gated O(1) hot path; composite-key uniqueness; chaos-tested 70h sleep/wake. Beyond the envelope, DLQ NDJSON catches every payload as recoverable text."

No literal "never disconnect" / "never fail" claims. TCP, kernel, remote processes, and disks can fail; we DETECT, ABSORB, AUDIT.

## Scope Summary

| Aspect | Today (Wave 4) | Wave 5 Plan |
|---|---|---|
| F&O subscription scope | 216 stocks ATM±25 + 3 indices full chain (~24,324) | NIFTY+BANKNIFTY+SENSEX all expiries/all strikes only (~10,783 F&O + 206 cash + 29 IDX_I = **11,018**) |
| Main-feed connections | 5 conns @ 97% cap, segment-bucket | 5 conns @ 44% cap, **category-balanced round-robin** (deterministic by security_id) |
| Depth-20 connections | 4 (NIFTY+BANKNIFTY mixed CE+PE), 98 instruments, ATM±24 | **5 conns: 4 single-side index + 1 top-50 from top-volume bucket sorted by `change_pct` DESC, SENSEX-skipped**, 246 instruments |
| Depth-200 connections | 4 static (NIFTY/BANKNIFTY ATM CE/PE) | **5 dynamic top-5 from top-volume bucket sorted by `change_pct` DESC, SENSEX-skipped** (1 per conn), 60s rebalance |
| Tokio worker affinity | 4 floating workers across 4 vCPUs | **4 workers pinned to dedicated cores** (0=WS, 1=pipeline, 2=ILP, 3=other) |
| candle_aggregator key | `HashMap<u32, _>` keyed on security_id alone | `HashMap<(u32, ExchangeSegment), _>` (I-P1-11) |
| tick_persistence flush failure | `warn!` (no Telegram) | `error!` (Loki → Telegram) per audit Rule 5 |

## Verified Live Numbers (QuestDB query 2026-04-30)

| Slot | Segment | Source | Count |
|---|---|---|---|
| Major index values | IDX_I | NIFTY=13, BANKNIFTY=25, SENSEX=51 | 3 |
| Display indices | IDX_I | sectoral + INDIA VIX | 26 |
| Cash equities | NSE_EQ | F&O underlying stocks (kept) | ~206 |
| F&O derivatives | NSE_FNO + BSE_FNO | NIFTY+BANKNIFTY+SENSEX, all expiries through 2030 | 10,783 |
| **TOTAL** | | | **~11,018** |

Equal-split per conn: 11,018 ÷ 5 = **2,204** (44% of Dhan's 5K/conn cap → 2.3× headroom).

## Depth-20 Split (5 conns, NSE-only)

| Conn | Underlying | Side | Strikes | Count | Source |
|---:|---|:---:|---|---:|---|
| 1 | NIFTY current expiry | CE only | ATM ±24 | 49 | depth_strike_selector |
| 2 | NIFTY current expiry | PE only | ATM ±24 | 49 | depth_strike_selector |
| 3 | BANKNIFTY current expiry | CE only | ATM ±24 | 49 | depth_strike_selector |
| 4 | BANKNIFTY current expiry | PE only | ATM ±24 | 49 | depth_strike_selector |
| 5 | Top 50 from top-volume bucket, sorted by `change_pct` DESC, SENSEX-skipped | mixed | dynamic 60s | 50 | option_movers (see Selector SQL below) |
| **Total** | | | | **246** | |

**SENSEX excluded from depth** — Dhan rule 13 (`full-market-depth.md`): "Only NSE segments valid. NSE_EQ and NSE_FNO only. BSE, MCX, Currency are NOT available." Selector skips any `exchange_segment = 'BSE_FNO'` row and takes next-eligible.

±24 locked (not ±25): Dhan caps 50 instruments per depth conn; ±25 = 51 → rejected.

## Depth-200 Split (5 conns × 1 instrument)

| Conn | Instrument | Source |
|---:|---|---|
| 1 | Top-volume + top `change_pct` #1 (NSE_FNO only) | option_movers (see Selector SQL below) |
| 2 | Top-volume + top `change_pct` #2 (NSE_FNO only) | dynamic 60s rebalance |
| 3 | Top-volume + top `change_pct` #3 (NSE_FNO only) | dynamic 60s rebalance |
| 4 | Top-volume + top `change_pct` #4 (NSE_FNO only) | dynamic 60s rebalance |
| 5 | Top-volume + top `change_pct` #5 (NSE_FNO only) | dynamic 60s rebalance |

Replaces static NIFTY ATM CE / NIFTY ATM PE / BANKNIFTY ATM CE / BANKNIFTY ATM PE.

## Selector SQL (depth-20 conn 5 + depth-200 conns 1-5)

The operator's literal: "always pick top volume section dude in that we need to sort by percentage bro i mean top percentage starting bro meanwhile see if it has sensex then we need to skip that and move onto the next one."

Mechanical interpretation:

1. Restrict to the **top-volume bucket** in `option_movers` (rows where `category = 'TOP_VOLUME'` — existing categorization in the movers writer).
2. Within that bucket, **sort by `change_pct` DESC** (highest gainers first).
3. **Exclude** any row where `exchange_segment = 'BSE_FNO'` (SENSEX). When a SENSEX row would have placed in the top-N, it is skipped and the next-eligible row takes its slot.
4. Final cut: `LIMIT 50` for depth-20 conn 5; `LIMIT 5` for depth-200 conns 1-5.
5. **Idempotent on rank churn:** unchanged set → no swap. Edge-triggered `Swap20` / `Swap200` only on rank-set change (set diff, not order diff).

```sql
-- depth-20 conn 5 (LIMIT 50); depth-200 conns 1-5 (LIMIT 5)
SELECT security_id, exchange_segment, underlying_symbol, change_pct, volume
FROM option_movers
WHERE category = 'TOP_VOLUME'
  AND exchange_segment <> 'BSE_FNO'   -- skip SENSEX (Dhan depth NSE-only)
  AND change_pct > 0                  -- gainers only
  AND volume > 0                      -- defensive — Open Question 1 default Yes
  AND ts > now() - 5m                 -- freshness window (movers writer cadence is 5s; 5m gives 60 snapshots tolerance)
ORDER BY change_pct DESC, volume DESC -- tie-break by volume to stay deterministic
LIMIT 50;                             -- LIMIT 5 for depth-200
```

If the result has < 50 (depth-20) or < 5 (depth-200), emit `DEPTH-20-DYN-03` / `DEPTH-200-DYN-01` (`Severity::High`) with `returned_count` and reason. Surviving conns keep last good set; we do NOT subscribe a degraded set.

## CPU Pinning (c7i.xlarge, 4 vCPU / 8 GB)

| Core | Workload | Why pinned |
|:---:|---|---|
| 0 | WS read loops + parser (5 main feed conns) | Hot path, must never preempt |
| 1 | Pipeline (tick_processor SPSC + candle aggregator + indicators) | 21-timeframe candle work |
| 2 | QuestDB ILP writer + rescue ring drain | Must drain ≥10K tps |
| 3 | API server, observability, auth, OMS, depth feeds, audit writers | Best-effort, degradation here doesn't block ticks |

## Resilience Envelope — How Each Operator Demand is Mechanically Backed

The operator's literal demands (zero ticks lost, never disconnect, never slow/locked/hanged, QuestDB never fails, O(1) latency, dedup uniqueness, real-time proof) are restated below with the **honest engineering guarantee** + **mechanical proof artefact** that fails the build on regression. No literal "never" without an envelope qualifier.

| Operator demand | Honest guarantee (envelope) | Mechanical proof artefact | Real-time check |
|---|---|---|---|
| Zero ticks lost | Bounded zero loss inside chaos-test envelope: rescue ring (600K) → spill file → DLQ NDJSON. Absorbs ≤60s QuestDB outage @ 10K tps. Beyond envelope, DLQ catches every payload as recoverable text. | `chaos_questdb_docker_pause.rs`, `chaos_rescue_ring_overflow.rs`, `chaos_zero_tick_loss.rs`, `resilience_sla_alert_guard.rs` | `tv_tick_dropped_total` Prom counter (= 0); `tv_questdb_disconnected_seconds > 30` alert |
| WebSocket never disconnects | DETECT ≤5s via pool watchdog; RECONNECT with `SubscribeRxGuard` preserving subs; SLEEP-UNTIL-OPEN post-close (Wave-2 WS-GAP-04); pool supervisor respawns dead tasks within 5s (WS-GAP-05). TCP failures cannot be prevented. | `test_subscribe_rx_guard_reinstalls_on_drop`, `test_subscribe_rx_guard_survives_many_cycles`, `test_pool_watchdog_task_accepts_health_status`, `respawn_dead_connections_loop` | `tv_websocket_connections_active` gauge (expect=5); `tv_ws_pool_respawn_total` rate; `WebSocketDisconnected` (in-market) / `WebSocketDisconnectedOffHours` (post-close) Telegram |
| Never slow/locked/hanged | Hot path DHAT-tested ≤4 alloc blocks / 8 KB across 10K calls; Criterion p99 ≤100ns enqueue; tick-gap >30s fires Telegram (60s coalesced); core_affinity pins 4 Tokio workers to dedicated cores (Item 6). | `quality/benchmark-budgets.toml` (5% regression block), `dhat_tick_parser.rs`, `dhat_tick_processor.rs`, `tick_gap_detector.rs`, `test_core_affinity_actually_pinned_via_proc_status` | `tv_tick_processing_duration_ns` histogram p99; `tv_tick_gaps_summary_count` |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ. Remote process failures cannot be prevented; we DETECT + ABSORB + ALERT. Schema self-heal at boot (`ALTER TABLE ADD COLUMN IF NOT EXISTS`). | `wait_for_questdb_ready` (BOOT-01/02), `tick_persistence rescue ring`, `chaos_questdb_docker_pause.rs`, `chaos_valkey_kill.rs` | `tv_questdb_disconnected_seconds`, `tv_rescue_ring_used_capacity`, `tv_spill_file_size_bytes`, BOOT-01/02 alerts |
| O(1) latency on hot path | Bounded constant-time ops: `from_le_bytes` parsing (no loops), `papaya` concurrent map for shared state, `Arc<HashMap>` immutable registry, SPSC 65K bounded channel. Bench-gate fails build on >5% regression. | `bench tick_binary_parse ≤10ns`, `bench papaya_lookup ≤50ns`, `bench oms_state_transition ≤100ns`, `quality/benchmark-budgets.toml` | `tv_tick_pipeline_routing_duration_ns` histogram |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 enforced everywhere. DEDUP UPSERT KEYS on every storage table; meta-guard scans every constant. | `dedup_segment_meta_guard.rs`, `subscription_planner regression tests`, `instrument_registry by_composite`, banned-pattern category 5 | `tv_instrument_registry_cross_segment_collisions` gauge |
| Real-time proof | 7-layer telemetry per new code path: typed event + ErrorCode + tracing+code + Prom counter + Grafana panel + alert rule + audit table. SLO-01/SLO-02 composite score @ 10s cadence. Market-open self-test @ 09:16:30 IST (SELFTEST-01/02). | `error_code_tag_guard.rs`, `error_code_rule_file_crossref.rs`, `operator_health_dashboard_guard.rs`, `resilience_sla_alert_guard.rs`, `slo_score.rs` | `mcp__tickvault-logs__run_doctor`, SLO-01/02 Telegram, MarketOpenStreamingConfirmation @ 09:15:30 |

## Common Runtime / Dynamic / Scalable Design (operator demand: "always make it common runtime dynamic scalable approach especially to add the stocks and its instruments later")

Goal: adding stocks (or any new underlying / segment) post-merge MUST be a config flip + universe rebuild + reconnect — **NEVER a code change**. The architecture below makes the universe data-driven from boot to runtime.

| Concern | How it stays common/runtime/dynamic | What an operator does to add stocks later |
|---|---|---|
| Subscription scope | `subscription.scope` enum in `config/base.toml` (Item 1). Today: `IndicesOnlyAllExpiries`. Tomorrow: `IndicesPlusStocks` / `FullUniverse` / future variants. | Edit `config/base.toml` → restart. No code change. |
| Universe filter | `subscription_planner::build_subscription_plan` reads scope enum + filters CSV by predicate. Predicate is a closure parameterised by scope — adding a scope = 1 new closure. | Implementer adds a `match` arm + filter closure for the new scope. |
| Underlying list | NIFTY/BANKNIFTY/SENSEX hardcoded today as `FULL_CHAIN_INDEX_SYMBOLS`. Wave 5 keeps it static for these 3, but the planner reads it at runtime. | When adding stocks F&O back, edit constant + flip scope enum. |
| Connection capacity | 5 conns × 5K cap = 25,000 max. Today 11,018 (44%). Adding 13,000 stock instruments → ~24,000 total → 48% per conn (still within 5K cap). | Capacity check in subscription_planner asserts `total ≤ MAX_TOTAL_SUBSCRIPTIONS = 25_000` at boot — fails fast if exceeded, no runtime surprise. |
| Distribution algorithm | Category-balanced round-robin keyed on `security_id`. Adding new instruments → they slot in via `i % 5` deterministically. | Zero operator action. Algorithm is data-driven. |
| Top-volume selector | SQL queries `option_movers` at runtime; new contracts auto-appear once the movers writer logs them. | Zero operator action. New contracts auto-rank. |
| Depth-20 conn 5 capacity | LIMIT 50 today. If we add stocks, the same selector still produces top-50 from the (now-bigger) bucket. | Zero change — selector handles N in, 50 out. |
| Depth-200 conn capacity | LIMIT 5 today. Fixed at 5 because Dhan caps depth-200 at 5 conns × 1 instr. | Hard ceiling — not configurable. |
| Core affinity | Reads `vcpu_count` at boot; pins to `min(vcpu_count, 4)` workers. Adding stocks doesn't change CPU pinning topology. | Zero operator action. |
| ErrorCode + Telegram | New events get a typed `NotificationEvent` variant + `ErrorCode` + runbook entry. Adding a new feature follows the 9-box checklist. | Standard 9-box per item, no architecture rework. |
| Audit tables | DEDUP UPSERT KEYS already pinned per table. Adding new tables = boot-time `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ADD COLUMN IF NOT EXISTS`. | Zero operator action — schema self-heal. |

**What is NOT scalable today (deferred for honesty):**
- 5 main-feed connections is a Dhan account limit. Cannot scale up without a new Dhan account or Dhan raising the cap.
- 5 depth-20 connections + 5 depth-200 connections — same Dhan limits.
- AWS c7i.xlarge fixed at 4 vCPUs by `aws-budget.md`. Scaling to c7i.2xlarge (8 vCPUs) is an operator decision + budget review.

## Ping/Pong Mechanism — Verified Mechanical Handling (operator demand: "respect dhan server ping pong precisely")

Source-verified against `crates/core/src/websocket/connection.rs` 2026-05-01:

| Step | Code line | Mechanical guarantee |
|---|---|---|
| Dhan server pings every 10s | per `live-market-feed.md` rule 16, `dhan-ref/03-live-market-feed-websocket.md` | External — verified by Dhan docs |
| `WebSocketStream::next()` delivers `Message::Ping(payload)` | connection.rs:1238 | tokio-tungstenite 0.29.0 surfaces the Ping to the application layer (does NOT auto-pong on its own) |
| App sends `Message::Pong(payload)` echoing the ping payload | connection.rs:1245 | Explicit `sink.send(Message::Pong(data))` — RFC 6455 compliant |
| Pong send bounded by `pong_timeout_secs` | connection.rs:1243 (default 5s, config-tunable) | If TCP buffer is stuck >5s the socket is treated as dead and we trigger reconnect — no silent hang |
| Last-message timestamp bumped on EVERY inbound frame | connection.rs:1052-1054 | Watchdog cannot false-positive during data-quiet periods because Pings keep the timer alive |
| Server disconnects after 40s with no pong (Dhan code 806) | per `dhan-ref/03` + `connection.rs:221` | We re-detect ≤5s via watchdog, reconnect with `SubscribeRxGuard`, replay subs |
| Server pong (when WE send a ping — we don't, but if we ever did) | connection.rs:1260 | `Message::Pong(_)` is logged + last-message bumped; no further action |
| Manual Ping frames | BANNED per `live-market-feed.md` rule 16. We rely on server-initiated pings. | Banned-pattern scanner blocks any `Message::Ping(...)` send |

**Why this is O(1) per ping:**
- 1 frame read (kernel-driven, epoll)
- 1 pattern match on `Message::Ping(_)`
- 1 atomic `AtomicU64::store` for last-message timestamp
- 1 `sink.send(Message::Pong(payload))` (bounded by `tokio::time::timeout`)
- No allocation, no clone (the payload is moved, not copied)
- Total: <10 µs typical, 5s hard ceiling on the timeout path

**What we never do for ping/pong:**
- No manual Ping frames (banned)
- No payload modification (echo verbatim per RFC 6455)
- No allocation in the Ping handler path
- No DB lookup or registry access

**Ratchet (existing, will be extended in Item 4/5):**
- `test_pong_send_bounded_by_pong_timeout`
- `test_last_message_timestamp_bumped_on_ping`
- Banned-pattern scanner category 7 blocks `Message::Ping(`

## Memory Management + Ring Buffer + WAL — 3-Tier Resilience Chain (operator demand: "manage memory and ring buffer wal")

Verified against `crates/storage/src/tick_persistence.rs` + `crates/common/src/constants.rs:1425`:

### Tier 1: Bounded ring buffer (in-memory, RAM-only)

| Property | Value | Source |
|---|---|---|
| Capacity | 600,000 ticks | `TICK_BUFFER_CAPACITY` (constants.rs:1425) |
| High watermark | 480,000 ticks (80%) | `TICK_BUFFER_HIGH_WATERMARK` (constants.rs:1431) |
| Backing structure | `VecDeque<Tick>` with pre-allocated capacity | tick_persistence.rs:265 |
| Push cost | O(1) amortised — pushes to back | VecDeque doc |
| Drain cost | O(N) on QuestDB recovery, but N is bounded | drain runs in background |
| Memory footprint | ~600K × ~80 bytes/tick = ~48 MB max | Bounded by capacity × Tick size |
| Time-to-fill @ 100K tps | 6 seconds | Fill rate / capacity |
| Time-to-fill @ 10K tps | 60 seconds | Matches QuestDB outage absorption envelope |
| Overflow behaviour | Spill to disk file (Tier 2) | tick_persistence.rs:561 |
| Drop behaviour | Drop happens ONLY when both ring + spill + DLQ fail simultaneously (extreme) | tick_persistence.rs:228 (DLQ counter) |

### Tier 2: Disk spill file (NDJSON, persistent across restarts)

| Property | Value |
|---|---|
| Format | NDJSON line per overflow tick (one row = one tick, every field) |
| Path | `data/spill/ticks-YYYYMMDD.bin` (tick_persistence.rs:7680 test ref) |
| Append cost | O(1) per tick (sequential file append, OS-buffered) |
| Pre-flight check | `Item 9 disk-full pre-flight`: if free bytes < threshold, abort before append (no half-written rows) |
| Recovery on restart | `recover_stale_spill_files()` reads + replays into ILP, deletes file (tick_persistence.rs:873) |
| WAL semantics | Yes — durable, append-only, replay-safe (DEDUP UPSERT KEYS handle duplicate replay) |
| Idempotency | DEDUP `(security_id, exchange_segment, ts, sequence_number)` makes re-replay safe |

### Tier 3: DLQ (Dead Letter Queue, last-resort recoverable text)

| Property | Value |
|---|---|
| Trigger | Both ring full AND spill write failed (e.g., disk full) |
| Format | NDJSON with full Tick payload + error context (tick_persistence.rs:222) |
| Path | `data/dlq/ticks-YYYYMMDD.ndjson` |
| Counter | `tv_dlq_ticks_total` (Prom) |
| Alert | DLQ count > 0 fires CRITICAL Telegram |
| Recovery | Manual operator intervention (parse + re-replay) |

### Memory governance (workspace-wide)

| Pattern | Where | Why |
|---|---|---|
| All channels are bounded | `tokio::sync::mpsc::channel(N)` everywhere | No unbounded growth |
| `VecDeque::with_capacity()` not `Vec::new()` | rescue ring + indicator snapshots | Pre-allocated, no realloc on hot path |
| `Arc<HashMap>` for read-mostly state | InstrumentRegistry, FnoUniverse | Shared, immutable, no lock contention |
| `papaya` concurrent map | SharedSpotPrices | Lock-free reads on hot path |
| `Bytes` for zero-copy network buffers | tokio-tungstenite frames | Reference-counted, no clone on broadcast |
| Stack-allocated structs | ParsedTick, OhlcvState | Zero heap, zero-alloc DHAT-pinned |
| `secrecy::Secret<String>` | Tokens, secrets | Auto-zeroize on drop |
| BANNED on hot path | `clone()`, `format!()`, `Vec::new()`, `Box`, `dyn` | Banned-pattern scanner blocks at commit |

### What "manage memory" means in production

- **Steady state:** rescue ring stays empty (drain matches inflow). Spill file does not exist.
- **Stress:** rescue ring fills to 480K (high watermark) → emit `tv_rescue_ring_high_watermark` Prom alert. Drain accelerates.
- **QuestDB outage ≤60s @ 10K tps:** ring absorbs entirely. Zero spill. Zero loss.
- **QuestDB outage 60s–10min:** ring fills, spill takes over. Disk-full pre-flight gates. Recovery replays.
- **Beyond envelope (disk full + QuestDB down):** DLQ catches as text. Operator sees CRITICAL Telegram. Recovery manual.

## Extreme Burst + Memory Pressure Defence — Stock-Cascade Lesson Learned

Operator's verbatim: "previously with stocks and its instruments we majorly faced the high memory issues and reconnection and disconnection many times — this affects our websockets live ticks majorly, biggest impact, shouldn't happen again."

**Root cause analysis (why Wave 4 with stocks blew up):**

| Failure mode | Mechanism | Wave 5 mitigation |
|---|---|---|
| Cardinality explosion | 24,324 instruments × 10 packets/sec/instr peak = 243K tps theoretical burst | Indices-only cuts to 11,018 — 55% reduction. Peak burst budget: ~110K tps. Ring (600K) still absorbs 5+ seconds. |
| Per-instrument memory | Each instrument has registry entries + spot-price slot + OHLCV state across 21 timeframes | 55% fewer instruments → 55% lower steady-state memory |
| OHLCV state map growth | `HashMap<u32, OhlcvState>` grew unbounded as stock options accumulated across expiries | Item 7: composite-key `HashMap<(u32, ExchangeSegment), OhlcvState>` + bounded by registry size |
| Channel saturation | Single mpsc(65,536) for ALL ticks; stocks at peak filled it faster than pipeline drained | Channel capacity unchanged but inflow halved → drain catches up reliably |
| QuestDB write amplification | Stock F&O wrote to `ticks` + every materialized view (21 timeframes × 22 movers categories × greeks) | Same per-tick cost, but 55% fewer ticks → ILP load halved |
| Reconnect storm cascade | Ring fills → backpressure on parser → parser falls behind → activity watchdog trips → reconnect → new subs → ring still full → loop | Item 6 core_affinity isolates parser CPU. Ring drains independent of WS read loop. |
| TCP RST from Dhan side | Stock F&O subscriptions hit Dhan's per-conn rate-limit on quote-mode aggregate updates → server forced disconnect | Indices-only stays well under per-conn 5K cap (44%); per-instrument update rate is lower for indices than stocks |
| Watchdog false positives | 1800s order-update watchdog tripped on idle accounts | Wave 2 fix: bumped to 14400s (4h) — already shipped, persists in Wave 5 |

**Wave 5 burst defence layers (cumulative):**

| Layer | What it does | Hard limit | Action on breach |
|---|---|---|---|
| 1. Subscription scope cap | Compile-time + boot-time assertion `total ≤ MAX_TOTAL_SUBSCRIPTIONS = 25_000` | 25K | Boot fails fast before subscribing |
| 2. Bounded SPSC channels | Every `mpsc::channel(N)` is bounded; `try_send` returns `Full` on overflow | 65,536 (parser→pipeline) | Increment `tv_tick_dropped_total`, drop the tick (last-resort) |
| 3. Rescue ring (Tier 1) | 600,000 ticks held in RAM if QuestDB writes block | 600K = ~48 MB | Spill to disk (Tier 2) |
| 4. Disk spill (Tier 2) | NDJSON append-only file | Disk-full pre-flight gate (Item 9 from prior wave, already shipped) | DLQ (Tier 3) |
| 5. DLQ (Tier 3) | Last-resort recoverable text | Disk capacity | CRITICAL Telegram + manual recovery |
| 6. High-watermark alert | Ring at 80% (480K) fires Prom alert | n/a | Operator paged before catastrophic fill |
| 7. Container memory limit | Docker `mem_limit` per `aws-budget.md` per-service budget | 7.4 GB total (4 GB QuestDB, 1 GB Valkey, etc.; tickvault binary ~1 GB) | Linux OOM-killer; PROC-01 ErrorCode reserved (`wave-4-error-codes.md`) |
| 8. cgroup OOM monitor | Phase 4-E1 (reserved): scrape `/sys/fs/cgroup/.../memory.events` | n/a | PROC-01 CRITICAL Telegram |
| 9. Backpressure on parser | Parser uses `try_send` not `send().await` — never blocks WS read loop | n/a | Drop + count, never stall |
| 10. Core affinity (Item 6) | WS read loop pinned to Core 0; ILP writer pinned to Core 2 — they cannot starve each other | n/a | If pinning fails: CORE-PIN-01 CRITICAL alert, app continues without pinning |

**Why this prevents the stock-cascade pattern:**

| Past failure step | Wave 5 break point |
|---|---|
| Step 1: Stocks F&O subscribed (24K instruments) | NOT REACHED — indices-only scope filters stock F&O out |
| Step 2: Peak burst @ 200K+ tps overwhelms parser | Inflow halved; parser stays current |
| Step 3: Ring fills past high-watermark | Earlier alert + `core_affinity` ensures drain task gets CPU |
| Step 4: Activity watchdog trips on parser stall | Parser doesn't stall (Core 0 dedicated); watchdog only trips on real disconnects |
| Step 5: Reconnect cascade across 5 conns | Even if 1 conn drops, surviving 4 absorb at 55%/conn (still under cap) |
| Step 6: OOM kill / Docker restart | PROC-01 monitor fires CRITICAL before kill; container `mem_limit` enforces |
| Step 7: Token expiry mid-cascade | AUTH-GAP-03 force-renewal-if-stale (Wave 2, shipped) |

**Real-time burst observability (during the next live test):**

| Prom counter / gauge | What it tells the operator |
|---|---|
| `tv_websocket_inbound_messages_per_sec{conn}` | Per-conn message rate; alerts on >2× steady-state |
| `tv_rescue_ring_used_capacity` | Ring fill level; alert at 80% (480K) |
| `tv_rescue_ring_high_watermark_breaches_total` | Counter — non-zero = burst pressure observed |
| `tv_tick_dropped_total{reason}` | reasons: `channel_full`, `dlq_persisted`, `dlq_failed` |
| `tv_spill_file_size_bytes` | Disk spill size in bytes |
| `tv_dlq_ticks_total` | DLQ count — non-zero = catastrophic fail, page operator |
| `tv_ws_pool_respawn_total` | Pool supervisor activity; non-zero rate = task panicking |
| `tv_questdb_disconnected_seconds` | QuestDB write latency; alert at >30s |
| `tv_resident_memory_bytes` (Phase 4-E3 reserved) | Process RSS; alert at 80% of cgroup limit |

**Burst chaos test (Item 11 — adding now):**

- [ ] **11. Stress test the burst defence chain**
- File: `crates/storage/tests/chaos_burst_indices_only.rs` (new)
- Synthetic: feed 200K tick/sec sustained for 10s into the parser channel; assert ring stays bounded, no DLQ writes, no parser stall
- Verifies: bounded channels, ring high-watermark alert, drain catches up post-burst
- Adds to existing chaos test suite alongside `chaos_questdb_docker_pause.rs`, `chaos_rescue_ring_overflow.rs`, `chaos_zero_tick_loss.rs`

**Honest non-claim:**

- We CANNOT promise "no OOM ever." The cgroup limit is set by Docker; if the operator deploys a wrong `mem_limit`, OOM-kill happens. PROC-01 detects + alerts, doesn't prevent.
- We CANNOT promise "no reconnect ever." Dhan can RST. We promise: detect ≤5s, reconnect with subs preserved, audit, sleep dormant post-close.
- We CAN promise: under the verified 11,018-instrument indices-only scope at observed peak rates (≤110K tps theoretical, typically ~30K tps), the chain absorbs without drop. Item 11 chaos test pins this envelope.

## Per-Packet O(1) Timing Budget Inside Every WS Read Loop

Operator demand: "achieve O(1) latency inside all the websockets and its connections."

What happens for ONE binary tick frame, from kernel readv() to QuestDB ILP append:

| Step | Operation | Cost | Hot-path-safe? |
|---|---|---|---|
| 1 | Kernel epoll wakes tokio runtime; `tokio-tungstenite` reads frame | ~1 µs (kernel + WS framing) | ✅ tokio internal |
| 2 | `Message::Binary(bytes)` delivered via `sink.next().await` | ~50 ns | ✅ no allocation, `Bytes` is ref-counted |
| 3 | Atomic `last_message_at.store(now_ns)` | ~5 ns (single atomic store) | ✅ |
| 4 | Header parse: 8 fixed `from_le_bytes` reads (response code, length, segment, security_id) | ~10 ns | ✅ DHAT-pinned |
| 5 | `match response_code { 1 => parse_index, 2 => parse_ticker, ... }` (jump table) | ~5 ns | ✅ |
| 6 | Fixed-offset payload parse (16 / 50 / 162 bytes via `from_le_bytes`) | ~10–30 ns | ✅ DHAT-pinned, no loops |
| 7 | Build stack-allocated `ParsedTick` struct | 0 (stack) | ✅ |
| 8 | `papaya::HashMap::get(&(security_id, segment))` for instrument metadata | ~50 ns avg, O(1) | ✅ Item 7 fix makes this composite-key |
| 9 | `tokio::sync::mpsc::Sender::try_send(parsed_tick)` to pipeline | ~100 ns | ✅ bounded SPSC, lock-free |
| 10 | If channel full: increment `tv_tick_dropped_total`, return | ~10 ns | ✅ counter only |
| 11 | If channel ok: pipeline drains in separate task (Core 1 if pinned) | (concurrent) | ✅ |
| **Total per tick on hot path** | | **~200–250 ns** | ✅ |

Then the pipeline task (Core 1) does:

| Step | Operation | Cost |
|---|---|---|
| 12 | Receive from SPSC channel | ~50 ns |
| 13 | Update OHLCV state in `HashMap<(u32, ExchangeSegment), OhlcvState>` (Item 7 fix) | ~80 ns avg, O(1) |
| 14 | Emit candle if minute boundary crossed | ~200 ns (rare path) |
| 15 | Push to ILP writer SPSC channel (Core 2 if pinned) | ~100 ns |

ILP writer task (Core 2):

| Step | Operation | Cost |
|---|---|---|
| 16 | Receive from ILP channel; build ILP line via `questdb-rs::Sender::row` | ~500 ns (sprintf-like, but bounded) |
| 17 | TCP send (kernel-buffered) | ~5 µs avg (network-bound, not blocking parser) |

**Bench-gated budgets (`quality/benchmark-budgets.toml`):**
- `tick_binary_parse` ≤10 ns (steps 4-7)
- `papaya_lookup` ≤50 ns (step 8)
- `tick_pipeline_routing` ≤100 ns (steps 9-12)
- `full_tick_processing` ≤10 µs (steps 1-17 end-to-end)

**Bench-gate** in `scripts/bench-gate.sh` fails the build if any benchmark regresses by >5%. Item 6 (`core_affinity`) closes the kernel-preemption variance source.

**What this is NOT:**
- NOT a literal "every tick guaranteed <250 ns end-to-end forever." OS scheduler can preempt. Core_affinity reduces variance; doesn't eliminate it.
- NOT zero-copy from kernel to ILP — there are 2 channel hops (parser→pipeline→ILP). The hops are ALL O(1) on a bounded channel.

## Dhan Ping/Pong + O(1) Inside Each WS Read Loop (Summary Quick-Ref)

Quick-reference table — see "Ping/Pong Mechanism" + "Per-Packet O(1) Timing Budget" above for full detail:

| Concern | Mechanical handling | Why it stays O(1) per packet |
|---|---|---|
| Server ping every 10s, timeout 40s | App-side explicit Pong reply with `pong_timeout_secs` ceiling (5s default). Bumps last-message atomic. | Pattern match + atomic store + bounded send |
| Frame parsing | 8-byte header + fixed-offset `from_le_bytes` per packet type | Constant-time per packet regardless of subscription count |
| Tick fan-out | SPSC 65,536-buffer `tokio::sync::mpsc` — `try_send` returns `Full` instead of awaiting | `try_send` is O(1); the channel is bounded |
| Subscribe/unsubscribe commands | `SubscribeRxGuard` reinstalls receiver on every read-loop exit | No DB lookup, no recompute — pre-computed at boot |
| Activity watchdog | 5s pool watchdog tick; per-conn last-message timestamp updated atomically (`AtomicU64`) | Single atomic load per conn per 5s |
| Audit row write | `WsReconnectAuditWriter` ILP append on every reconnect; failure routes to STORAGE-GAP-03/AUDIT-03 | ILP append is non-blocking |
| Order-update activity | 14400s (4h) watchdog (NOT 1800s) | Same pool watchdog tick |

**What we never do on the read loop:**
- No `clone()` (banned by hot-path scanner)
- No `Vec::new()` / `format!()` / `.collect()` (banned)
- No DB lookups (registry is `Arc<HashMap>`)
- No mutex acquisition (`papaya` for shared state)
- No `dyn` dispatch (banned on hot path)

**Honest non-claim:** TCP RST storms, kernel scheduler preemption, and remote-process failures CAN happen. Our envelope is: detect ≤5s, reconnect with subs preserved, audit every event, sleep dormantly post-close. Item 6 (core_affinity) closes the last preemption gap.

## Top Movers Under Wave 5 — Indices-Only Scope

Operator question: "what about movers — top movers and all of it section?"

The `option_movers` + `stock_movers` writers already exist with 7 ranking categories. Wave 5 keeps both writers active; the universe shrinks but the snapshotting cadence + categories stay identical. The 22-timeframe redesign (`active-plan-movers-22tf-redesign-v2.md`, APPROVED parallel plan) is independent — Wave 5 does not block or duplicate it.

### Verified categories (source: `crates/api/src/handlers/market_data.rs:360-368`)

| Category | What it ranks |
|---|---|
| `HIGHEST_OI` | Top open interest contracts |
| `OI_GAINER` | Largest OI delta over snapshot window |
| `OI_LOSER` | Largest OI drop over snapshot window |
| `TOP_VOLUME` | Highest volume contracts |
| `TOP_VALUE` | Highest notional value (price × volume) |
| `PRICE_GAINER` | Largest % price increase |
| `PRICE_LOSER` | Largest % price decrease |

### Wave 5 movers behaviour

| Concern | Today | Wave 5 |
|---|---|---|
| `option_movers` universe | All NSE_FNO + BSE_FNO (~22K stock F&O + ~10K index F&O = ~32K) | NSE_FNO + BSE_FNO for NIFTY/BANKNIFTY/SENSEX only (~10,783) — drops ~22K stock F&O entries |
| `stock_movers` universe | All NSE_EQ F&O underlyings (~206) | Same ~206 (cash equities still subscribed) — UNCHANGED |
| Snapshot cadence | 5s per writer (option) / 60s (stock) | Same |
| Storage tables | `option_movers`, `stock_movers`, plus `top_movers_22tf` (in-flight) | Same — no schema change |
| Selector for depth-20 conn 5 + depth-200 | Wave 5 NEW: `category='TOP_VOLUME'` then sort by `change_pct` DESC, SENSEX-skip | Documented in "Selector SQL" section above |
| Memory cost | High (32K × 7 categories × 5s cadence) | ~3× cheaper (10,783 × 7 × 5s) — fits the burst-defence envelope |

### Pre-open movers (`PreopenMoversTracker`) under Wave 5

- Continues to fire 09:00–09:13 IST per `MOVERS-03` runbook (`wave-1-error-codes.md`).
- Universe: 218 stocks + 2 indices (existing). Wave 5 keeps cash equities so this is unchanged.
- SENSEX (BSXOPT) special case: `phase = 'PREOPEN_UNAVAILABLE'` rows continue (BSE has no formal pre-open auction at the same window) — not a Wave 5 regression.

### Movers writers under indices-only — what we add to the plan

- [ ] **Item 12. Wire option_movers selector to use Wave 5 universe filter.** No new code; verify `option_movers` writer already iterates only the subscribed instruments — if it scans ALL NSE_FNO+BSE_FNO contracts regardless of subscription, that's wasted work and should narrow.
- File: `crates/core/src/pipeline/option_movers.rs` (verify, possibly amend)
- Test: `test_option_movers_universe_matches_subscription_set_under_indices_only_scope`
- 9-box: gauge `tv_option_movers_universe_size` (expect ~10,783 under indices-only); alert if >12,000 (regression to old universe)

## Previous Close Routing — Wave 5 Per-Slot Map

Operator question: "how about prev close?" Per `dhan/live-market-feed.md` rule + Dhan Ticket #5525125:

| Slot | Segment | Subscribed feed mode | Where prev close lives | Parser |
|---|---|---|---|---|
| Major indices | IDX_I | Ticker (forced) | **Standalone PrevClose packet (code 6, 16 bytes, bytes 8-11 f32 LE)** | `parse_previous_close_packet` (`crates/core/src/parser/previous_close.rs`) |
| Display indices | IDX_I | Ticker (forced) | Same as major indices | Same |
| Cash equities | NSE_EQ | **Quote** (Wave 5 change — see "Feed Mode Per Slot" below) | Quote packet bytes 38-41 f32 LE (close field) | `parse_quote_packet` |
| Index F&O derivatives | NSE_FNO | Full | Full packet bytes 50-53 f32 LE (close field) | `parse_full_packet` |
| Index F&O derivatives | BSE_FNO (SENSEX) | Full | Same as NSE_FNO | Same |

**Hard rule (per `live-market-feed.md` `MECHANICAL RULE: Previous-Day Close Routing`):**

> "Subscribing to Ticker mode on equities/derivatives and then waiting for code 6 packets is a bug — those packets will never arrive. The symptom is 'prev close missing for 24,972 of 25,000 instruments, only 28 IDX_I indices have it'."

**Wave 5 must verify on every boot:**

| Check | What it ensures |
|---|---|
| For every NSE_EQ subscription: feed_mode is Quote or Full | Otherwise prev close = always 0 |
| For every NSE_FNO/BSE_FNO subscription: feed_mode is Full | Quote-mode derivatives miss OI + close at correct offset |
| For every IDX_I subscription: feed_mode is Ticker AND we listen for code 6 packets | Standalone PrevClose packet is the ONLY source for indices |

### What we add to the plan

- [ ] **Item 13. Boot-time prev-close routing assertion**
- File: `crates/app/src/main.rs` (boot sequence) + `crates/core/src/instrument/subscription_planner.rs`
- Tests: `test_idx_i_subscriptions_use_ticker_mode`, `test_nse_eq_subscriptions_use_quote_or_full`, `test_nse_fno_bse_fno_subscriptions_use_full_mode`
- ErrorCode: `PREVCLOSE-03` (new — boot-time invariant violation, Severity::Critical, halts boot)
- 9-box: ratchet test that scans the computed subscription plan and fails build if any slot/feed-mode combo is wrong

## Feed Mode Per Slot — Ticker / Quote / Full Decision Matrix

Operator question: "how about ticker, quote and full packet how bro?"

### Packet sizes + content (verified per `dhan/live-market-feed.md` rules 5-10)

| Mode | Packet size | Content | Has prev close? |
|---|---|---|---|
| Ticker (code 2) | 16 bytes (8 header + 8 payload) | LTP + LTT only | NO — but IDX_I gets standalone code 6 separately |
| Quote (code 4) | 50 bytes (8 header + 42 payload) | LTP + LTQ + LTT + ATP + Volume + buy/sell qty + day OHLC + **close at bytes 38-41** | YES (NSE_EQ) |
| OI (code 5) | 12 bytes | OI only — auxiliary packet alongside Quote on F&O | N/A |
| Full (code 8) | 162 bytes (8 header + 154 payload) | Quote fields + OI + 5-level depth + **close at bytes 50-53** | YES (NSE_FNO/BSE_FNO) |
| PrevClose (code 6) | 16 bytes | Indices ONLY — prev close + prev day OI | YES (IDX_I) |

### Wave 5 feed mode allocation (FINAL)

| Slot | Count | Feed mode | Bytes per packet | Justification |
|---|---|---|---|---|
| IDX_I major | 3 | Ticker (forced) | 16 | LTP for ATM math; prev close from code 6; depth not needed |
| IDX_I display | 26 | Ticker (forced) | 16 | LTP for dashboard; prev close from code 6 |
| NSE_EQ cash | ~206 | **Quote** (downgrade from Full) | 50 | Need volume + OHLC + prev close (38-41); 5-level depth not needed (cash isn't in any depth subscription) |
| NSE_FNO derivatives | ~7,500 | Full | 162 | Need OI + 5-level depth (for option chain UI) + prev close (50-53) |
| BSE_FNO derivatives (SENSEX) | ~2,283 | Full | 162 | Same as NSE_FNO |

### Bandwidth math at peak burst

| Slot | Count | Bytes/pkt | Pkts/sec/instr peak | MB/sec total | MB/sec/conn (÷5) |
|---|---|---|---|---|---|
| IDX_I (29) | 29 | 16 | 10 | 0.0046 | 0.0009 |
| NSE_EQ (~206) | 206 | 50 | 5 | 0.052 | 0.010 |
| NSE_FNO+BSE_FNO (~10,783) | 10,783 | 162 | 5 | 8.74 | 1.75 |
| **TOTAL** | 11,018 | | | **8.79 MB/sec** | **1.76 MB/sec/conn** |

c7i.xlarge has 12.5 Gbps network bandwidth → 1.56 GB/sec → we use 0.56% of network at peak. Comfortable.

### Memory math at peak

| Slot | Count | Bytes/instr in registry + state | MB |
|---|---|---|---|
| IDX_I | 29 | ~500 (lookup + spot map + OHLCV state × 21 timeframes) | 0.014 |
| NSE_EQ | 206 | ~10K (registry + spot + OHLCV × 21 + indicator engine state) | 2.06 |
| NSE_FNO+BSE_FNO | 10,783 | ~12K (above + greeks + OI tracker + 5-level depth state) | 129 |
| **TOTAL state** | | | **~131 MB** |
| Rescue ring (TICK_BUFFER_CAPACITY=600K × ~80 bytes) | | | 48 |
| Subscription plan (Arc<Vec<Message>>) | | | ~5 |
| Other (audit writers, Telegram queue, etc.) | | | ~50 |
| **TOTAL tickvault binary RSS estimate** | | | **~234 MB** |

vs c7i.xlarge 8 GB RAM (with QuestDB at 4 GB, Valkey 1 GB, others ~2 GB → tickvault gets ~1 GB). 234 MB / 1 GB = 23% — comfortable headroom for bursts.

### What we add to the plan

- [ ] **Item 14. NSE_EQ feed_mode downgrade Full → Quote**
- File: `crates/core/src/instrument/subscription_planner.rs`
- Tests: `test_nse_eq_uses_quote_mode_under_indices_only`, `test_quote_packet_close_field_matches_prev_close_lookup`
- 9-box: Prom gauge `tv_subscription_bytes_per_sec_per_conn{conn}` — alert if any conn exceeds 5 MB/sec sustained (means we accidentally subscribed Full to cash); `FeedModeRoutingAudit` typed event at boot listing per-slot mode counts; ratchet test

## Previous Close Storage Optimization — Drop Redundant Table Writes for Quote/Full

Operator question: "for full we don't need to store the day close or prev close into a separate table right dude?"

**Answer: correct, with a per-segment carve-out.**

### Current redundancy

| Segment | Feed mode | Close field location | Currently writes to `previous_close` table? | Redundant? |
|---|---|---|---|---|
| IDX_I | Ticker | NOT in tick — only code-6 packet | YES (`PrevCloseSource::Code6`) | NO — only source |
| NSE_EQ | Quote | Quote packet bytes 38-41 (rides every tick into `ticks.day_close`) | YES (`PrevCloseSource::QuoteClose`) | YES — `day_close` column in `ticks` already has it |
| NSE_FNO + BSE_FNO | Full | Full packet bytes 50-53 (rides every tick into `ticks.day_close`) | YES (`PrevCloseSource::FullClose`) | YES — `day_close` column in `ticks` already has it |

### Architecture history

- Originally: `previous_close` table populated by all 3 routes
- Then: deprecated under "day_close from Full ticks suffices" (`tick_persistence.rs:432`)
- Then: un-deprecated under "movers writers need fast lookup on day-boundary restart" (`previous_close_persistence.rs:14-22`)
- Wave 5: **drop Quote/Full routes; keep only IDX_I (Code6) + small in-memory cache**

### Why dropping Quote + Full is safe

1. The `close` value (bytes 38-41 / 50-53) is **identical for every tick within a trading day** — it's a static "previous day's close" baked into the packet. So writing it on every tick is wasteful (DEDUP UPSERT dedupes to 1 row per day, but ILP append cost is N writes).
2. The `ticks.day_close` column already preserves this value indexed by `(security_id, exchange_segment, ts)`. A movers writer can read the LATEST tick's `day_close` field to get the previous-day close for that instrument — O(1) via the existing `Arc<HashMap>` `prev_close` in-memory cache (`prev_close_writer.rs`).
3. The `first_seen_set` already prevents writing more than once per `(trading_date, security_id, segment)` — but the architecture still does N appends before the dedup gate. Dropping the call site removes the cost entirely.

### Why IDX_I must keep the table (or its in-memory equivalent)

- Ticker mode (16 bytes) has NO close field. There is no `day_close` column populated for IDX_I ticks.
- The standalone code-6 packet arrives intermittently (often only once per day at market-open). If the app boots after that one packet has fired, there's no way to recover the value from incoming ticks.
- Either: keep the `previous_close` table populated only for IDX_I rows (29 rows/day max), OR keep the in-memory cache via `prev_close_writer.rs` and persist to QuestDB only for restart recovery.
- Wave 5 picks: **keep in-memory cache for IDX_I; reduce table to IDX_I-only rows for restart-recovery**.

### What we add to the plan

- [ ] **Item 15. Drop `previous_close` table writes for NSE_EQ + NSE_FNO + BSE_FNO**
- Files: `crates/core/src/pipeline/prev_close_persist.rs`, `crates/storage/src/previous_close_persistence.rs`, `crates/core/src/parser/dispatcher.rs`
- Tests: `test_prev_close_persist_skips_quote_and_full_sources`, `test_prev_close_table_only_contains_idx_i_rows`, `test_movers_writer_reads_day_close_from_ticks_via_in_memory_cache`
- Behaviour change:
  - Parser dispatcher: Quote (code 4) + Full (code 8) ticks STILL emit `prev_close` to the in-memory `Arc<HashMap>` cache (used by movers writers for change_pct computation in O(1))
  - Parser dispatcher: STOP routing Quote + Full close values to `previous_close_persist::send`
  - PrevClose (code 6) for IDX_I: continues to route to both in-memory cache AND `previous_close` table (the table is the restart-recovery source; cache is the runtime source)
- Storage:
  - `previous_close` table size shrinks from ~11K rows/day to ~29 rows/day (IDX_I only)
  - ILP append rate drops by ~99.7% on the prev_close path
  - Movers writers unchanged — they read from the in-memory cache, which still gets all 3 segments
- Boot recovery:
  - On startup, in-memory cache is rehydrated from `previous_close` table for IDX_I + from the latest tick per instrument for NSE_EQ + NSE_FNO + BSE_FNO (single QuestDB query: `SELECT security_id, exchange_segment, day_close FROM ticks WHERE ts >= $today_ist_midnight AND ts < $tomorrow_ist_midnight ORDER BY ts DESC LIMIT 1 PER instrument`)
- 9-box: ErrorCode N/A (cleanup, not new failure path); Prom counter `tv_prev_close_persist_writes_total{source}` — expect `code6` only after migration; alert if any `quote_close` / `full_close` increments post-Wave-5; ratchet test at module level.

### Why not delete `previous_close` table entirely?

| Consideration | If we delete | If we keep IDX_I-only |
|---|---|---|
| IDX_I restart recovery | Need a different durable source (e.g., dedicated cache file) | Already works |
| Movers writers | Already read in-memory cache (no table dependency) | Same |
| Schema simplicity | -1 table | +29 rows/day, simpler change |
| Operator audit query "what was yesterday's NIFTY close?" | Query `ticks` table for last tick of yesterday IDX_I — slower | Direct lookup, fast |

**Decision: keep IDX_I-only.** Smaller change, preserves audit queries.

## Plan Items (now 15, was 10)

Each item carries the 9-box checklist per `stream-resilience.md` B8: ① typed event, ② ErrorCode, ③ tracing+code field, ④ Prometheus counter, ⑤ Grafana panel, ⑥ alert rule, ⑦ call site, ⑧ triage YAML rule, ⑨ ratchet test.

### - [ ] 1. `subscription.scope` config gate

- Files: `crates/common/src/config.rs`, `config/base.toml`
- Tests: `test_subscription_scope_enum_indices_only_all_expiries_default`, `test_subscription_scope_round_trips_via_figment`
- Add enum `SubscriptionScope::{IndicesOnlyAllExpiries, FullUniverse}`. Default = `IndicesOnlyAllExpiries`.
- 9-box: ① N/A (config) ② N/A ③ N/A ④ `tv_subscription_scope` info-gauge ⑤ Operator Health header ⑥ N/A ⑦ `subscription_planner::build_subscription_plan` ⑧ N/A ⑨ enum tests + figment round-trip

### - [ ] 2. Universe filter — keep 11,018 instruments

- Files: `crates/core/src/instrument/subscription_planner.rs`
- Tests: `test_indices_only_scope_filters_to_three_underlyings`, `test_universe_count_pinned_at_11018`, `test_finnifty_midcpnifty_excluded_from_indices_only`, `test_stock_fno_excluded_under_indices_only_scope`
- Predicate: `instrument.underlying_symbol IN ('NIFTY','BANKNIFTY','SENSEX')` for derivatives. Cash equities + IDX_I unchanged.
- Drops Phase 2 dispatcher (09:13 IST) + Mode C live-tick ATM resolver + pre-open REST `/marketfeed/ltp` fallback to inert (no stock F&O).
- 9-box: ① `Phase2Skipped` (new, Severity::Info, fires once at 09:13:00 explaining "no stock F&O under indices-only scope") ② N/A (no failure path) ③ tracing `info!(scope = "indices_only", count = 11018)` at boot ④ `tv_subscription_total_instruments` gauge ⑤ Operator Health "Subscription scope" panel ⑥ `tv-subscription-count-drift` (alert if `tv_subscription_total_instruments` outside 10,500..11,500) ⑦ `main.rs` boot sequence ⑧ N/A ⑨ count-pinned test

### - [ ] 3. Main feed equal-split (5 × ~2,204, category-balanced round-robin)

- Files: `crates/core/src/websocket/connection_pool.rs`, `crates/core/src/instrument/subscription_distribution.rs` (new)
- Tests: `test_distribution_is_category_balanced_round_robin`, `test_same_security_id_lands_on_same_connection_across_runs`, `test_distribution_per_conn_within_5_pct_of_target`, `test_distribution_idempotent_on_replay`
- Algorithm: group by [IDX_I, NSE_EQ, NSE_FNO+BSE_FNO]; sort each group by security_id ASC; `conn_index = i % 5`. Stable across boots.
- 9-box: ① N/A ② N/A ③ tracing `info!(conn = i, count = n)` per conn at boot ④ `tv_main_feed_per_conn_instrument_count` gauge with `{conn}` label ⑤ Operator Health "Main feed distribution" stacked-bar panel ⑥ `tv-main-feed-conn-overload` (any conn > 4,500) ⑦ `connection_pool::distribute` ⑧ N/A ⑨ deterministic-replay test + spread test

### - [ ] 4. Depth-20 5-conn split (4 single-side index + 1 top-50 gainers)

- Files: `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/instrument/depth_strike_selector.rs`, `crates/core/src/instrument/depth_20_top_gainers.rs` (new), `crates/app/src/main.rs`
- Tests: `test_depth_20_conn_1_is_nifty_ce_atm_24`, `test_depth_20_conn_2_is_nifty_pe_atm_24`, `test_depth_20_conn_3_is_banknifty_ce_atm_24`, `test_depth_20_conn_4_is_banknifty_pe_atm_24`, `test_depth_20_conn_5_is_top_50_volume_gainers`, `test_depth_20_total_under_dhan_50_per_conn_cap`, `test_depth_20_excludes_sensex`
- Top-50 selector queries `option_movers` every 60s. SQL is the canonical "Selector SQL" block above (top-volume bucket → sort by `change_pct` DESC → SENSEX-skipped → LIMIT 50). Edge-triggered swap via existing `DepthCommand::Swap20` only when the set diff is non-empty (rank-set churn, not order churn).
- 9-box: ① `Depth20TopSetEmpty` (existing DEPTH-DYN-01 reused — fires when result < 50) + new `Depth20TopGainersSwapped` (Severity::Low, edge-triggered on rank change) ② `DEPTH-20-DYN-03` (top-50 selector empty/below capacity, severity High) ③ `error!(code = ErrorCode::Depth20Dyn03TopGainersEmpty.code_str())` ④ `tv_depth_20_top_gainers_set_size` gauge, `tv_depth_20_top_gainers_swaps_total` counter ⑤ Operator Health "Depth-20 top-50 gainers" panel ⑥ `tv-depth-20-dyn-03-empty-set` (gauge < 25 for > 5min during market hours) ⑦ `main.rs::run_depth_20_top_gainers_loop` ⑧ `.claude/triage/error-rules.yaml::depth-20-dyn-03-top-set-empty-escalate` ⑨ all 7 ratchet tests above + `test_top_50_query_filters_change_pct_positive`

### - [ ] 5. Depth-200 dynamic top-5 (replaces static ATM CE/PE)

- Files: `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/instrument/depth_200_top_gainers.rs` (new), `crates/app/src/main.rs`
- Tests: `test_depth_200_picks_top_5_by_volume`, `test_depth_200_filters_change_pct_positive`, `test_depth_200_one_instrument_per_conn`, `test_depth_200_swap_on_rank_change`, `test_depth_200_excludes_sensex`
- Replaces existing static `["NIFTY", "BANKNIFTY"]` ATM CE/PE config in `main.rs:2151,3113,3685`. Uses existing `DepthCommand::Swap200` path. 60s rebalance from `option_movers` using the canonical "Selector SQL" block above (top-volume bucket → sort by `change_pct` DESC → SENSEX-skipped → LIMIT 5). Edge-triggered swap on rank-set churn only.
- 9-box: ① new `Depth200TopGainersSwapped` (Severity::Low, edge-triggered) + reuse `Depth200SwapChannelBroken` (existing DEPTH-DYN-02) ② `DEPTH-200-DYN-01` (top-5 selector returned < 5, severity High) ③ `error!(code = ErrorCode::Depth200Dyn01TopSetEmpty.code_str())` ④ `tv_depth_200_top_gainers_set_size`, `tv_depth_200_top_gainers_swaps_total` ⑤ Operator Health "Depth-200 top-5" panel ⑥ `tv-depth-200-dyn-01-empty-set` ⑦ `main.rs::run_depth_200_top_gainers_loop` ⑧ `.claude/triage/error-rules.yaml::depth-200-dyn-01-top-set-empty-escalate` ⑨ all 5 tests above

### - [ ] 6. Wire `core_affinity` — pin 4 Tokio workers to 4 vCPUs

- Files: `crates/app/src/main.rs`, `crates/app/src/runtime.rs` (new), `Cargo.toml` (already has core_affinity 0.8.3)
- Tests: `test_core_affinity_actually_pinned_via_proc_status`, `test_runtime_builder_sets_worker_count_to_vcpu_count`, `test_pinning_skipped_gracefully_on_single_vcpu_host`, `test_each_worker_has_unique_cpu_set`
- Use `tokio::runtime::Builder::new_multi_thread().worker_threads(4).on_thread_start(|| { core_affinity::set_for_current(...) })`. Read `/proc/self/task/*/status` post-boot; assert `Cpus_allowed_list` is single-cpu per worker.
- 9-box: ① `CorePinningFailed` (Severity::High) ② `CORE-PIN-01` (pinning failed at boot, severity High), `CORE-PIN-02` (worker drifted off pinned core, severity Medium) ③ `error!(code = ErrorCode::CorePin01PinningFailedAtBoot.code_str())` ④ `tv_core_pinning_workers_pinned_total` gauge (expect = 4) ⑤ Operator Health "Core affinity" panel ⑥ `tv-core-pin-01-pinning-failed` (gauge != 4 at boot) ⑦ `main.rs::build_runtime` ⑧ `.claude/triage/error-rules.yaml::core-pin-01-pinning-failed-at-boot-escalate` ⑨ all 4 tests above + boot-time assertion

### - [ ] 7. Fix CRITICAL: candle_aggregator segment-aware key (I-P1-11)

- Files: `crates/core/src/pipeline/candle_aggregator.rs:12,21` (and any other site)
- Tests: `test_candle_aggregator_keyed_on_security_id_and_segment`, `test_two_instruments_same_id_different_segment_do_not_merge_ohlcv` (regression for FINNIFTY=27 IDX_I vs NSE_EQ=27 collision)
- Migrate `HashMap<u32, OhlcvState>` → `HashMap<(u32, ExchangeSegment), OhlcvState>`. Update banned-pattern scanner glob to include `crates/core/src/pipeline/candle_aggregator.rs`.
- 9-box: ① N/A ② reuse I-P1-11 ③ N/A (lookup, no error) ④ `tv_candle_aggregator_keyspace_size` gauge ⑤ existing I-P1-11 panel ⑥ N/A ⑦ `tick_processor::on_tick` ⑧ N/A ⑨ regression test on collision pair

### - [ ] 8. Fix HIGH: `warn!` → `error!` at tick_persistence.rs:357

- Files: `crates/storage/src/tick_persistence.rs:357`
- Tests: `crates/storage/tests/error_level_meta_guard.rs` (existing meta-guard catches new violations going forward; one-time fix is the line itself)
- Add `code = ErrorCode::StorageGap03AuditWriteFailure.code_str()` field to satisfy tag-guard.
- 9-box: ① existing typed event ② STORAGE-GAP-03 (existing) ③ added in this fix ④ existing counter ⑤ existing panel ⑥ existing alert ⑦ tick_persistence flush call site ⑧ existing triage rule ⑨ meta-guard catches future regressions

### - [ ] 9. New ErrorCode variants (4)

- Files: `crates/common/src/error_code.rs`, `.claude/rules/project/wave-5-error-codes.md` (new)
- Tests: existing `error_code_rule_file_crossref.rs` + `error_code_tag_guard.rs` cover all 4 automatically
- Variants:
  - `CorePin01PinningFailedAtBoot` → `code_str() = "CORE-PIN-01"`, Severity::High, runbook `wave-5-error-codes.md`
  - `CorePin02WorkerDrifted` → `"CORE-PIN-02"`, Severity::Medium
  - `Depth20Dyn03TopGainersEmpty` → `"DEPTH-20-DYN-03"`, Severity::High
  - `Depth200Dyn01TopGainersEmpty` → `"DEPTH-200-DYN-01"`, Severity::High (reuse if existing variant of same `code_str()` already exists; check before adding)
- 9-box: ① N/A ② self ③ N/A ④ N/A ⑤ N/A ⑥ N/A ⑦ added by Items 4/5/6 ⑧ entries in `error-rules.yaml` per Item 4/5/6 ⑨ enum invariant tests + cross-ref test + tag-guard

### - [ ] 10. Adversarial 3-agent re-review on the diff

- Spawn `hot-path-reviewer`, `security-reviewer`, `general-purpose` (hostile bug-hunt) in parallel against the final diff before opening PR. Per `wave-4-shared-preamble.md` Section 3.
- Fix every CRITICAL and HIGH inline. Document every false-positive triage with grep evidence.

## Verification (before PR)

```bash
cargo check --workspace
cargo test -p tickvault-common --lib   # ErrorCode invariants
cargo test -p tickvault-core --lib     # subscription_planner + depth selectors
cargo test -p tickvault-storage --lib  # tick_persistence flush guard
cargo test -p tickvault-app --lib      # core_affinity pinning + runtime
FULL_QA=1 make scoped-check
bash .claude/hooks/banned-pattern-scanner.sh
bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all
bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"
bash .claude/hooks/plan-verify.sh
cargo bench                            # hot path touched (Item 7)
cargo test --features dhat             # zero hot-path allocations
```

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 09:00 IST with `subscription.scope = "indices_only_all_expiries"` | Universe loads 11,018; 5 main-feed conns at ~2,204 each; depth-20 wires 5 conns (4 NIFTY/BANKNIFTY single-side + 1 top-50); depth-200 wires 5 conns to top-5 gainers; 4 Tokio workers pinned to cores 0-3 |
| 2 | option_movers TOP_VOLUME bucket sorted by change_pct DESC returns < 50 after SENSEX-skip during market hours | DEPTH-20-DYN-03 fires `Severity::High` Telegram with `returned_count` + reason (`empty_after_sensex_skip` / `bucket_below_capacity`); surviving conn 5 keeps last good set |
| 3 | option_movers TOP_VOLUME bucket sorted by change_pct DESC returns < 5 after SENSEX-skip during market hours | DEPTH-200-DYN-01 fires `Severity::High`; surviving 5 conns keep last good gainers |
| 11 | SENSEX (BSE_FNO) appears in top-volume + top change_pct rank | Selector skips, takes next-eligible NSE_FNO row; no Telegram (normal behavior) |
| 12 | TOP_VOLUME bucket has rank churn but final set unchanged after sort + SENSEX-skip | No Swap command issued (idempotent on set, not order); zero unnecessary disconnects |
| 4 | core_affinity::set_for_current returns false on one worker | CORE-PIN-01 fires `Severity::High`; gauge `tv_core_pinning_workers_pinned_total < 4` |
| 5 | candle_aggregator receives tick with security_id=27 from IDX_I + tick with security_id=27 from NSE_EQ | OHLCV state stays in two distinct keys; regression test pinned |
| 6 | tick_persistence flush fails | `error!` (not `warn!`) emitted with `code = "STORAGE-GAP-03"`; Loki routes to Telegram via Alertmanager |
| 7 | One main-feed conn drops mid-day | Surviving 4 absorb 11,018 instruments at 55%/conn (still < 5K cap); SubscribeRxGuard reinstates subscriptions |
| 8 | Two main-feed conns drop simultaneously | Surviving 3 absorb at 73%/conn; 1,328 free slots remain — no Dhan rejection |
| 9 | Top-50 ranking changes (rank 47 falls out, new entry rises) | Single `Swap20` command on conn 5; `Depth20TopGainersSwapped` Severity::Low Telegram (edge-triggered) |
| 10 | Tokio worker drifts off pinned core | CORE-PIN-02 fires `Severity::Medium`; counter `tv_core_pinning_drift_total` increments |

## Open Questions for Operator

| # | Question | Default if unanswered |
|---|---|---|
| 1 | ~~Top-50 / Top-5 query — should we ALSO require `volume > 0`?~~ **ANSWERED 2026-05-01:** Selector restricted to `category = 'TOP_VOLUME'` bucket, then sorted by `change_pct` DESC, SENSEX (BSE_FNO) skipped. `volume > 0` defensive guard included. Canonical SQL pinned in "Selector SQL" section above. | (resolved) |
| 2 | If on-day-1-boot the `option_movers` table is empty (e.g., first 60s after universe build), should depth-20 conn 5 + depth-200 5 conns: (a) skip subscribe and wait, or (b) fall back to NIFTY/BANKNIFTY ATM as today? | (a) skip + retry every 60s; INFO Telegram once |
| 3 | core_affinity on dev Mac (typically 8-12 cores) vs AWS c7i.xlarge (4 vCPUs) — do we hard-fail boot on Mac if `vcpu_count != 4`? | No, pin to first 4; Mac is dev-only |

## Notes

- Phase 2 dispatcher (09:13 IST) becomes inert under indices-only scope — keep code path, gate on `config.subscription.scope`. Do NOT delete; reactivating full universe must be a 1-line config flip.
- `MidMarketBootComplete` event becomes inert (no stock F&O Mode C resolution needed). Keep variant; rename trigger condition.
- Pre-open buffer + REST fallback (`preopen_rest_fallback.rs`) becomes inert for stocks. Indices still use it.
- DEDUP keys + I-P1-11 composite-key invariants UNCHANGED. New code MUST use `(security_id, exchange_segment)`.
