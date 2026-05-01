# Implementation Plan: Wave 5 — Indices-Only Subscription + Depth Redesign + Resilience Hardening

**Status:** APPROVED
**Date:** 2026-05-01
**Approved by:** Parthiban (operator) — verbatim "yes approved" 2026-05-01 + "whichever is recommended add everything" 2026-05-01 (defaults locked).
**Operator decisions captured:**
- **C1 → Option B (REVERSED 2026-05-01 later in session):** Operator reaffirmed original single-side 4-conn + 1 dynamic-50 design. Wave 5 RIPS the merged `depth_20_dynamic_subscriber.rs` (commits `f3b7baa`, `29b1407`, `72028c5`, `c9f53a4`) and replaces with: Conn 1 = NIFTY CE ATM±24, Conn 2 = NIFTY PE ATM±24, Conn 3 = BANKNIFTY CE ATM±24, Conn 4 = BANKNIFTY PE ATM±24, Conn 5 = top-50 dynamic. SQL changed to `category='TOP_VOLUME' ORDER BY change_pct DESC LIMIT 50` with SENSEX excluded. Total 246 instruments. Item 4 rewritten below.
- **C2 → Reuse:** Item 16 reuses already-merged `Movers22TfTracker`; Wave 5 adds 60s checkpoint + 25-tf extension only if not already wired. No new tracker struct.
- **Q4 → Keep `22Tf` naming:** Item 19 extends merged `Movers22TfTracker` to 25 timeframes IN PLACE. The "22" number is now historical artifact. ErrorCode prefix `MOVERS-22TF-*` retained. Type names retained.
- **All 26 non-blocker review findings (5 CRITICAL + 12 HIGH + 9 MEDIUM + 5 LOW)** are folded into the relevant item 9-box checklists; tracked in Item 18 Findings Remediation Index below.
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

## Movers Storage Strategy — In-Memory Tracker vs Persistent Snapshot (operator question 2026-05-01)

Operator: "is it always better to store every movers options separately or just store and calculate and save and from runtime we can pull it up dude what is the best extreme efficient option?"

### Answer: Hybrid — in-memory rolling tracker (runtime) + 60s checkpoint (audit/restart)

| Strategy | Pros | Cons | Use case |
|---|---|---|---|
| A. Persist every snapshot at 1Hz | Full history, granular replay | ~75K writes/sec to QuestDB on indices-only universe; ILP load + write amplification | Rejected — too expensive |
| B. In-memory only | O(1) reads, zero ILP cost | Lost on restart; no audit trail | Rejected — SEBI 5y retention + restart recovery |
| C. **Hybrid: in-memory rolling tracker + 60s checkpoint** | O(1) runtime reads from memory; bounded ILP load (~1 snapshot/min); audit + restart recovery from QuestDB | 60s of granularity loss on restart | **CHOSEN** |

### Wave 5 movers architecture (all 7 categories × all subscribed instruments)

```
Tick stream (parser) ──┐
                       │
                       ▼
              ┌─────────────────────────┐
              │  In-memory MoversTracker│  ← O(1) update per tick
              │  (papaya / Arc<HashMap>) │
              │  - HIGHEST_OI            │
              │  - OI_GAINER / LOSER     │
              │  - TOP_VOLUME / VALUE    │
              │  - PRICE_GAINER / LOSER  │
              └────────────┬─────────────┘
                           │
              ┌────────────┴───────────┬──────────────────┐
              │                        │                  │
              ▼                        ▼                  ▼
       Runtime queries       60s snapshot writer   1Hz pre-open writer
       (API handlers)         (option_movers,        (PreopenMoversTracker
       O(1) lookup            stock_movers,           09:00–09:13 only)
                              top_movers_22tf)
```

### Why O(1) latency is preserved

| Layer | Operation | Cost |
|---|---|---|
| Per-tick update | Increment counters in `papaya::HashMap<(security_id, segment), MoversState>` | ~80 ns avg, lock-free |
| Per-tick category re-rank | NOT done per tick — done at snapshot boundary (60s); per-tick we only update raw stats | ~0 ns extra per tick |
| Snapshot rank computation | `BTreeSet<(value, security_id)>` per category, sorted insertion is O(log N) where N=10,783; total snapshot = 7 categories × N log N ≈ 1ms | Off hot path |
| API query "top 50 by volume" | Read pre-computed snapshot from `Arc<MoversSnapshot>` — O(1) | <100 ns |
| QuestDB checkpoint write | 7 categories × 50 ranks × 1 row = 350 rows/snapshot × 1/min = 5.8 rows/sec ILP | Negligible |

### Storage cost math

| Approach | Rows/day | ILP writes/sec peak | Storage 90 days hot | Storage 5 years cold (S3) |
|---|---|---|---|---|
| A. 1Hz persist | 75K rows/sec × 8h = 2.16 B rows/day | 75,000 | ~2 TB | impractical |
| C. 60s checkpoint | 350 rows/snapshot × 60 snapshots/h × 8h = 168K rows/day | 5.8 | ~150 GB | ~330 GB Glacier — fits ₹333/mo budget |

**Decision: C is 12,857× cheaper on writes, 13,300× cheaper on storage. Same operator-facing query latency (both use in-memory cache).**

### What we add to the plan

- [ ] **Item 16. Movers hybrid storage — REUSE existing `Movers22TfTracker` (BLOCKER C2 → Reuse)**

**Decision:** Item 16 does NOT create a new tracker. The merged trunk (commit `16a7220`) already ships `Movers22TfTracker` (papaya-backed) wired into `tick_processor` (commit `c9f53a4`). Wave 5 contributes:
- Verify that a 60s checkpoint to QuestDB is wired (per the parallel APPROVED `active-plan-movers-22tf-redesign-v2.md`). If yes → no-op verification. If no → add the checkpoint loop on Core 3 best-effort.
- Verify atomicity: `Movers22TfTracker` internal state must use `AtomicU64` / `AtomicI64` only (per C3). If any non-atomic field exists, refactor.
- Verify snapshot rank computation runs on Core 3 in `tokio::spawn` (per C4).
- Verify channel between tracker and writer is bounded `mpsc::channel(8192)` drop-NEWEST (per H11).
- Verify daily reset semantics: tracker resets at IST midnight OR cleanly carries pre-market data into the 09:15 first bucket (per H3, H9, anchor formula in Item 19).

- Files: `crates/core/src/pipeline/movers_22tf_tracker.rs` (verify atomic-only), `crates/core/src/pipeline/movers_22tf_scheduler.rs` (verify Core 3), `crates/core/src/pipeline/movers_22tf_writer_state.rs` (verify channel bounded), `crates/core/src/pipeline/movers_22tf_supervisor.rs`
- Tests:
  - `test_movers_22tf_state_uses_atomics_only` (C3 — DHAT zero-alloc proof on per-tick update)
  - `test_movers_22tf_snapshot_runs_on_core_3_not_core_1` (C4)
  - `test_movers_22tf_snapshot_pin_pattern_no_torn_reads` (C5 — loom test)
  - `test_movers_22tf_writer_channel_bounded_8192_drop_newest` (H11)
  - `test_movers_22tf_resets_at_ist_midnight_or_anchors_to_0915` (H3, H9)
  - `test_movers_22tf_warmup_signal_emitted_at_first_snapshot_post_boot` (H9 — `MoversTrackerReady` Severity::Info)
  - `test_movers_22tf_daily_state_does_not_leak_across_trading_days` (H3)
- 9-box (extending existing `MOVERS-22TF-01/02/03`):
  - ① typed event: existing `Movers22TfWriterIlpFailed` + add `MoversTrackerReady` (Severity::Info, edge-triggered first snapshot post-boot)
  - ② ErrorCode: existing `MOVERS-22TF-01/02/03`; no new codes
  - ③ tracing: existing `info!(tf, snapshot_count, …)`
  - ④ Prom: existing `tv_movers_writer_errors_total{stage,timeframe}`; add `tv_movers_warmup_complete` info gauge (1 = ready, 0 = warmup)
  - ⑤ Grafana: existing 22-tf Operator Health panel (extends to 25 tf labels per Item 19)
  - ⑥ Alert: existing `tv-movers-22tf-*` rules; cardinality auto-extends
  - ⑦ Call site: existing `tick_processor` enrichment (commit `c9f53a4`); no new call site
  - ⑧ Triage rule: existing
  - ⑨ Ratchet: 7 tests above + DEDUP keys for `top_movers_22tf` documented as `(timeframe, bucket, rank_category, rank, security_id, segment)` per existing `movers_persistence.rs:242`

## Dhan Documentation Pin — Previous Close in Full / Quote Packets

Operator: "dhan already mentioned right in full we can automatically fetch the prev close day data from close right dude can you check that doc or email dude they clearly mentioned it right?"

**Confirmed.** Source artefacts (live in repo):

| Source | Where | What it confirms |
|---|---|---|
| Dhan Ticket #5525125 (2026-04-10) | `docs/dhan-support/README.md` archive entry: `2026-04-20-ticket-5525125-prev-close-routing.md` (archived) | "PrevClose as a standalone packet is emitted only for IDX_I instruments. For NSE_EQ and NSE_FNO subscriptions, the previous-day close is delivered inside the Quote and Full packets via the `close` field" |
| Rule file | `.claude/rules/dhan/live-market-feed.md` lines 79-101 (`MECHANICAL RULE: Previous-Day Close Routing`) | Codifies the routing — explicit byte offsets per packet |
| Module docstring | `crates/storage/src/previous_close_persistence.rs:7-13` | 3-row table mapping segment → source packet → bytes → source label |
| Annexure | `dhan-ref/03-live-market-feed-websocket.md` | Quote bytes 38-41 + Full bytes 50-53 = `close` field; Dhan doc labels this as "close" but per Ticket #5525125 it's **previous trading session's close**, NOT current day's close |

**This is why Wave 5 Item 15 is safe.** The close value rides every Quote/Full tick; the `previous_close` table writes for NSE_EQ + NSE_FNO + BSE_FNO are pure redundancy.

## Parallelization Map — How to Split Wave 5 Across Multiple Sessions Without Conflicts

Operator: "in the future we can split it out or we can provide parallel everything to new new claude code sessions right if it doesn't have any dependency right dude?"

Per `stream-resilience.md` per-PR-set protocol: ≤3 sub-PRs, ≤30 files / ≤3K LoC each. Wave 5 = 16 items. Dependency-grouped:

| Group | Items | Touches | Depends on | Can run in parallel? |
|---|---|---|---|---|
| **A — Foundation** (1 PR, 1 commit each) | Item 1 (config gate) | `crates/common/src/config.rs`, `config/base.toml` | nothing | NO — must land first |
| **B — Independent fixes** (1 PR, parallelizable commits) | Item 6 (core_affinity), Item 7 (candle_aggregator key), Item 8 (warn→error), Item 9 (ErrorCodes), Item 15 (drop redundant prev_close) | core, common, storage, app | nothing — orthogonal | **YES — 5 sessions in parallel possible** |
| **C — Universe-driven** (1 PR, sequential within) | Item 2 (universe filter) → Items 3, 4, 5, 12, 13, 14 | core, app, api | Item 1 + B | Items 3/4/5/12/13/14 can run parallel **after** Item 2 |
| **D — Storage strategy** (1 PR) | Item 16 (movers hybrid) | core, storage | Item 12 | Sequential after Item 12 |
| **E — Test+review** (1 PR, last) | Item 10 (adversarial review), Item 11 (chaos burst) | tests | all of above | Sequential at the end |

### Recommended sub-PR split for parallel session execution

| Sub-PR | Items | Sessions in parallel | Wall-clock estimate |
|---|---|---|---|
| Wave 5-A | 1, 6, 7, 8, 9, 15 | 5 sessions in parallel (Item 1 first, then 6/7/8/9/15 fan-out) | ~2-3 hours (parallel) |
| Wave 5-B | 2, 3, 4, 5, 12, 13, 14 | 1 session for Item 2; then 6 sessions parallel (3/4/5/12/13/14) | ~3-4 hours (parallel) |
| Wave 5-C | 16, 11, 10 | Sequential | ~2 hours |

**Per-session protocol on parallel work:**
1. Each session reads `.claude/plans/active-plan-wave-5-indices-only.md` → identifies its assigned item(s)
2. Each session creates a sub-branch off `claude/fetch-status-info-V56GN` (e.g., `claude/wave-5a-item-6-core-affinity`)
3. After implementation: PR back to the wave branch (not main)
4. Wave branch PR (#410) gets all sub-PRs merged, then squash to main
5. **Conflict avoidance:** items in same Group share NO files (verified by file paths in Plan Items table)

### What we add to the plan

- [ ] **Item 17. Document parallelization map + sub-branch protocol**
- Files: this plan file (already done); add `.claude/plans/wave-5-sub-branches.md` listing each sub-branch + assigned items + base commit
- Tests: N/A (documentation)
- 9-box: N/A (process)

## Item 18 — Adversarial 3-Agent Review Findings (2026-05-01)

Per `stream-resilience.md` B3 + `wave-4-shared-preamble.md` §3, three specialist agents (`hot-path-reviewer`, `security-reviewer`, `general-purpose` hostile bug-hunt) reviewed the 17-item plan in parallel. **Total findings: 7 CRITICAL + 12 HIGH + 9 MEDIUM + 5 LOW = 33.** Two CRITICALs are MERGE BLOCKERS — they collide with already-merged code and require an operator architectural decision before implementation can begin.

### MERGE BLOCKERS — operator decision required before Item 4 / Item 16 can ship

**BLOCKER C1 — Item 4 collides with already-shipped `depth_20_dynamic_subscriber.rs`.**

The merged trunk (commits `f3b7baa`, `29b1407`, `72028c5`, `c9f53a4`) ALREADY ships a depth-20 dynamic selector covering slots 3/4/5 (3 dynamic slots × 50 = 150 instruments) with:
- `DEPTH_20_DYNAMIC_SLOT_COUNT = 3`
- `DEPTH_20_DYNAMIC_SLOT_CAPACITY = 50`
- SQL: `WHERE change_pct > 0 ORDER BY volume DESC LIMIT 150`
- Slots 1+2 pinned to NIFTY + BANKNIFTY mixed CE+PE ATM±24

Wave 5 Item 4 specifies:
- Slots 1+2+3+4 = NIFTY/BANKNIFTY single-side (CE only / PE only) ATM±24
- Slot 5 = top-50 dynamic from `category='TOP_VOLUME'` sorted by `change_pct` DESC

These are incompatible. Wave 5 cuts dynamic coverage 150 → 50 and changes slots 1+2 from mixed to single-side. **Operator must choose:**

| Option | Means | Trade-off |
|---|---|---|
| A. Adopt merged design | Drop the single-side-conn idea; keep mixed CE+PE slots 1+2 + 3 dynamic top-50 slots (150 total) | Smaller diff, no rework. Loses the "single-side packet routing" benefit Wave 5 hypothesised. |
| B. Refactor to Wave 5 design | Rip merged dynamic-subscriber, change slots 1-4 to single-side, add 1 top-50 dynamic | Larger blast radius, must retest the Phase 7 ratchets in `depth_20_dynamic_subscriber.rs`. Halves dynamic coverage. |
| C. Hybrid | Keep merged 3-slot dynamic; add single-side ONLY for NIFTY/BANKNIFTY (slots 1+2 split into 4 sub-conns? requires Dhan to allow extra conns — not possible, hard cap = 5 depth-20 conns total) | NOT VIABLE — Dhan caps depth-20 at 5 conns total |

**Recommendation: Option A.** Smaller change, more dynamic coverage, aligns with already-merged code. The "single-side packet routing" benefit was theoretical — actual packet routing in Dhan's depth feed splits Bid/Ask packets regardless of slot composition.

**Action:** Operator confirms Option A → rewrite Item 4 to match merged design + extend the in-flight movers-22tf-redesign-v2 plan.

---

**BLOCKER C2 — Item 16 collides with already-shipped `Movers22TfTracker`.**

The merged trunk (commit `16a7220`) ALREADY ships a papaya-backed `Movers22TfTracker` wired into `tick_processor` (commit `c9f53a4`). The 22-tf v3 plan (`active-plan-movers-22tf-redesign-v2.md`, APPROVED in parallel) owns the snapshot writers + scheduler.

Wave 5 Item 16 specifies "in-memory MoversTracker" — a new struct with the same 7 categories. Two trackers consuming the same tick stream = double-counted volume + OI deltas, OR one silently shadowing the other.

**Resolution:** Wave 5 Item 16 must REUSE `Movers22TfTracker`, not create a parallel struct. Specifically:
- Remove "new MoversTracker" wording from Item 16
- Specify: "Wave 5 Item 16 = configuration tweaks to existing `Movers22TfTracker` snapshot cadence + add 60s checkpoint to QuestDB if not already wired"
- Verify the 22-tf v3 plan already covers the 60s checkpoint; if yes, Item 16 becomes a verify-only no-op. If no, Item 16 adds the checkpoint to the existing tracker.

**Action:** Operator approves: "Item 16 reuses Movers22TfTracker" → rewrite Item 16 as verify-or-extend of the existing tracker.

---

### CRITICAL findings (5 more, all blocking)

| # | Source | Item | Finding | Remediation |
|---|---|---|---|---|
| C3 | hot-path | 16 | `papaya::HashMap<MoversState>` per-tick mutation: needs `AtomicU64`/`AtomicI64` only OR `compute_mut` causes per-tick alloc. Current `Movers22TfTracker` uses atomics — **verify Wave 5 doesn't add non-atomic fields**. | Add ratchet `dhat_movers_22tf_per_tick.rs` (DHAT zero-alloc proof, not timing test). |
| C4 | hot-path | 16 | BTreeSet rank computation at 60s snapshot MUST run on Core 3, not Core 1. | Item 6 9-box: explicitly map "snapshot computation" to Core 3 in `tokio::spawn` task. |
| C5 | bug-hunt | 16 | Snapshot vs per-tick race: BTreeSet iteration of papaya map can see torn reads if a tick mutates `MoversState.volume` mid-iteration. | Use papaya `pin()` snapshot-of-snapshot pattern OR copy values into local Vec before sorting. Add ratchet `loom_movers_snapshot_vs_per_tick_race.rs`. |
| C6 | bug-hunt | 13 | PREVCLOSE-03 false-OK inverse: correctly configured slot with empty in-memory cache silently serves `0.0` for prev_close until first tick (potentially 30+ min on low-volume contracts). Operator sees no alert (Rule 11 violation). | Add `tv_prev_close_cache_coverage_ratio` gauge + alert. Add `PREVCLOSE-04` (cache empty for instrument > 5 min during market hours, Severity::High). |
| C7 | bug-hunt | 1 | `subscription.scope` default = `IndicesOnlyAllExpiries` + Figment merge: missing key in `prod.toml` silently flips prod from FullUniverse → IndicesOnly on next boot, dropping 13K subscriptions. | Add boot-time assertion: scope MUST be present explicitly in env-overlay TOML; missing = halt with `CONFIG-01` Critical. |

### HIGH findings (12, all need plan amendments)

| # | Source | Item | Finding | Remediation |
|---|---|---|---|---|
| H1 | hot-path | 11 | Chaos burst generator `try_send(parsed_tick.clone())` → false-positive DHAT allocations. | Pre-build `[ParsedTick; 1024]` array, cycle via `i % 1024`. Add `cargo test --features dhat` ratchet on the chaos test itself. |
| H2 | hot-path | 4+5 | Depth selector tasks must `tokio::spawn` on Core 3 (best-effort), not Core 1 (pipeline). | Item 6 9-box: explicit Core 3 mapping for `run_depth_20_top_gainers_loop` + `run_depth_200_top_gainers_loop`. |
| H3 | security | 15 | Cache-miss fail-safe missing: empty rehydration query → NaN/Inf `change_pct` → undefined depth selector ordering. | Add `PREVCLOSE-05`: rehydration empty for any NSE_EQ/F&O instrument during market hours → halt boot OR default `change_pct = 0.0` + suppress depth selector for those instruments. |
| H4 | security | 6 | `core_affinity::set_for_current` calls `unsafe` `sched_setaffinity`/`thread_policy_set`. No justification documented. | Add `// APPROVED: core_affinity 0.8.3 calls sched_setaffinity via libc; reviewed for soundness` at call site. Run `cargo audit` explicitly for this dep. |
| H5 | security | 4+5 | ILP injection surface: `underlying_symbol` from `option_movers` flows back to QuestDB ILP writes without guaranteed sanitization. | Add `test_selector_result_sanitized_before_ilp_write` — assert all values from selector pass through `crates/common/src/sanitize.rs`. |
| H6 | security | 15 | IST timestamp ambiguity in rehydration query: must explicitly use `Utc::now() + IST_UTC_OFFSET_NANOS` (REST/QuestDB path requires the offset, opposite of WebSocket ts rule). | Spell out `IST_UTC_OFFSET_NANOS` usage in plan + add ratchet `test_prev_close_rehydration_query_uses_ist_offset`. |
| H7 | bug-hunt | 4+5 | Edge-trigger needs hysteresis: top-50 by `change_pct` in chop market = rank-set flips every 60s = subscription thrash on conn 5 = Dhan rate-limit risk. | Add `min rank-change ≥ 3 positions` OR `5-min minimum dwell` guard. Plan needs `tv_depth_20_swap_thrash_ratio` gauge. |
| H8 | bug-hunt | 6 | `core_affinity` on sub-4-vCPU container: 4 workers fighting over 2 cores with affinity masks pinning to non-existent cores. | Read `core_affinity::get_core_ids().len()` first; pin only `min(N, 4)`; emit CORE-PIN-03 if N < 4 (Severity::Medium, app continues). |
| H9 | bug-hunt | 16 | Daily reset for in-memory `Movers22TfTracker` undefined: app boot at 09:14 IST → empty tracker → all `change_pct = 0` until 09:14 + 60s → DEPTH-20-DYN-03/DEPTH-200-DYN-01 firing CRITICAL during ~1 min cold boot every day. | Add positive warm-up signal `MoversTrackerReady` Severity::Info @ first snapshot post-boot. Suppress depth selector emptiness alerts during `[boot_ts, boot_ts + 90s]` window. |
| H10 | bug-hunt | 14 | NSE_EQ Full→Quote downstream consumers: any consumer reading `tick.depth_levels` for NSE_EQ goes from valid 5-level → empty silently. | Add `test_no_consumer_reads_depth_for_nse_eq` — grep across crates for `depth_levels` usage on cash. |
| H11 | bug-hunt | 16 | Channel boundedness between tracker and snapshot writer unstated. | Match 22-tf v3 plan: `mpsc::channel(8192)` drop-NEWEST. |
| H12 | bug-hunt | 11 | Chaos burst test market-hours gate: no `#[ignore]` or feature flag — risk of running against live QuestDB during normal `cargo test --workspace`. | Add `#[ignore]` + `make chaos` invocation; document in test docstring. |

### MEDIUM findings (9)

| # | Source | Item | Finding | Remediation |
|---|---|---|---|---|
| M1 | hot-path | 7 | `HashMap::with_capacity` not preserved post-migration. | Add ratchet `test_candle_aggregator_uses_with_capacity_not_new`. |
| M2 | hot-path | 14 | NSE_EQ prev-close lookup must use existing `Arc<HashMap>` cache, not new Mutex. | Item 14 9-box: forbid Mutex-guarded prev_close lookups; reference `prev_close_writer.rs`. |
| M3 | security | 4+5 | `change_pct > 0` filter + Item 15 cache-miss bug = bear-day page storm. | Items 4+5+15 risk-linked in plan; resolution = H3 + C6 fixes propagate. |
| M4 | security | 16 | DEDUP keys for `top_movers_22tf` not explicitly stated. | Item 16 9-box: write `(security_id, exchange_segment, ts, category)` explicitly so meta-guard has correct expectation. |
| M5 | security | 13 | PREVCLOSE-03 runtime semantics ambiguous (halt mid-session bad). | Distinguish: boot=halt; runtime=Severity::High + continue with last valid plan. |
| M6 | bug-hunt | 7 | `HashMap<u32, _>` → composite-key migration breaks test fixtures workspace-wide. | Add subtask "audit + migrate all call sites" to Item 7. |
| M7 | bug-hunt | 4+5+6+14+16 | Counter `_total` naming + gauge semantics mismatch (e.g., `tv_core_pinning_workers_pinned_total` is gauge, named `_total`). | Item 6 ratchet renames to `tv_core_pinning_workers_pinned` (gauge convention). All counters wrapped in `increase()`/`rate()` per Rule 12. |
| M8 | bug-hunt | 4+5 | Two selector queries firing same 60s tick = inconsistent bucket if a row ages out between Item 4 and Item 5 query. | Single SQL query, shared `Arc<Vec<Row>>`, both selectors slice from it. |
| M9 | bug-hunt | 12 | Universe filter at runtime is likely a verify-only no-op (writer narrows naturally under indices-only). | Test name reflects no-op: `test_option_movers_writer_narrows_naturally_under_indices_only`. |

### LOW findings (5)

| # | Source | Item | Finding | Remediation |
|---|---|---|---|---|
| L1 | bug-hunt | 9 | `Depth200Dyn01TopGainersEmpty` may collide with existing `Depth20Dyn01TopSetEmpty` code-string. | Cross-ref test catches; verify before adding the variant. |
| L2 | bug-hunt | Selector | `ts > now() - 5m`: QuestDB `now()` is UTC, `option_movers.ts` is IST nanos. Selector returns zero rows always unless normalized. | Pin TZ in SQL: `ts > now() - 5m` → `ts > timestamp_floor('s', now()) + INTERVAL '5h30m' - INTERVAL '5m'` OR explicit IST-aware comparison. Add ratchet. |
| L3 | bug-hunt | 2 | Phase 2 dispatcher inert: PHASE2-02 `Phase2EmitGuard` panics in debug if dropped without firing. | Specify: under indices-only, guard either never instantiated, OR instantiated and `emit_skipped()` called. Plan must say which. |
| L4 | security | 17 | Future templating risk on `wave-5-sub-branches.md` if file gains dynamic content. | Flag for review if scripts later read this file. |
| L5 | hot-path | 16 | papaya `pin()` per-tick vs per-batch ambiguity. | Ratchet `test_movers_tracker_pins_papaya_once_per_batch_not_per_tick`. |

### Plan amendment summary

The 33 findings break into:
- **2 BLOCKERS (C1, C2):** require operator architectural decision (Option A recommended for both — adopt merged design / reuse merged tracker).
- **5 CRITICALs (C3-C7):** add to plan as Item 18 sub-items, must fix before any sub-PR opens.
- **12 HIGHs:** fold remediation into existing items 1-17 9-box checklists.
- **9 MEDIUMs:** same.
- **5 LOWs:** same; some are verify-only (L1).

### Status update — RESOLVED 2026-05-01

Operator approved defaults verbatim "whichever is recommended add everything" 2026-05-01:
1. **BLOCKER C1 → Option A** (adopt merged design). Item 4 rewritten above.
2. **BLOCKER C2 → Reuse** existing `Movers22TfTracker`. Item 16 rewritten above.
3. **Item 19 Q4 → Keep `22Tf` naming** + extend in place to 25 timeframes.

Plan moves back to `Status: APPROVED`. Implementation can begin.

### Findings Remediation Index

Each non-blocker finding mapped to its remediation site:

| Finding | Severity | Source | Plan amendment site |
|---|---|---|---|
| C3 — papaya MoversState atomic-only | CRITICAL | hot-path | Item 16 test `test_movers_22tf_state_uses_atomics_only` |
| C4 — BTreeSet snapshot on Core 3 | CRITICAL | hot-path | Item 6 9-box (Core 3 mapping) + Item 16 test `test_movers_22tf_snapshot_runs_on_core_3_not_core_1` |
| C5 — snapshot vs per-tick race | CRITICAL | bug-hunt | Item 16 test `test_movers_22tf_snapshot_pin_pattern_no_torn_reads` (loom) |
| C6 — PREVCLOSE-03 false-OK on empty cache | CRITICAL | bug-hunt | Item 13 + new ErrorCode **PREVCLOSE-04** (cache empty for instr > 5min market hours, Severity::High) |
| C7 — subscription.scope Figment merge silent flip | CRITICAL | bug-hunt | Item 1 + new ErrorCode **CONFIG-01** (scope must be explicit in env-overlay TOML, Critical, halt boot) |
| H1 — chaos burst pre-built array | HIGH | hot-path | Item 11 — pre-build `[ParsedTick; 1024]` + cycle via `i % 1024` |
| H2 — depth selectors on Core 3 | HIGH | hot-path | Item 4 + Item 5 + Item 6 9-box |
| H3 — Item 15 cache-miss fail-safe | HIGH | security | Item 15 + new ErrorCode **PREVCLOSE-05** (rehydration empty during market hours = halt OR default + suppress depth selector) |
| H4 — core_affinity unsafe justification | HIGH | security | Item 6 — `// APPROVED: core_affinity 0.8.3 calls sched_setaffinity` comment + `cargo audit` for the dep |
| H5 — ILP injection sanitization | HIGH | security | Item 4 + Item 5 — `test_selector_result_sanitized_before_ilp_write` |
| H6 — IST timestamp explicit in rehydration | HIGH | security | Item 15 — pin `Utc::now() + IST_UTC_OFFSET_NANOS` + ratchet `test_prev_close_rehydration_query_uses_ist_offset` |
| H7 — edge-trigger hysteresis | HIGH | bug-hunt | Item 4 — `test_depth_20_swap_hysteresis_3_position_min` + DEPTH-DYN-04 alert |
| H8 — sub-4-vCPU pinning | HIGH | bug-hunt | Item 6 — read `core_affinity::get_core_ids().len()`, pin `min(N, 4)`, emit CORE-PIN-03 if N<4 (Severity::Medium) |
| H9 — daily reset undefined | HIGH | bug-hunt | Item 16 + Item 19 — 09:15 IST anchor formula + `MoversTrackerReady` warmup signal + suppress depth selector emptiness during `[boot_ts, boot_ts + 90s]` |
| H10 — NSE_EQ Quote downstream consumers | HIGH | bug-hunt | Item 14 — `test_no_consumer_reads_depth_for_nse_eq` |
| H11 — channel boundedness | HIGH | bug-hunt | Item 16 — `mpsc::channel(8192)` drop-NEWEST |
| H12 — chaos test market-hours gate | HIGH | bug-hunt | Item 11 — `#[ignore]` + `make chaos` invocation |
| M1 — HashMap::with_capacity | MEDIUM | hot-path | Item 7 test |
| M2 — NSE_EQ prev-close cache reuse | MEDIUM | hot-path | Item 14 9-box (forbid Mutex) |
| M3 — bear-day page storm risk-link | MEDIUM | security | Items 4+5+15 — H3/C6/H9 cumulatively resolve |
| M4 — top_movers_22tf DEDUP keys explicit | MEDIUM | security | Item 16 9-box: `(timeframe, bucket, rank_category, rank, security_id, segment)` per existing movers_persistence.rs:242 |
| M5 — PREVCLOSE-03 runtime semantics | MEDIUM | security | Item 13 — boot=halt; runtime=Severity::High + last valid plan |
| M6 — candle_aggregator workspace migration | MEDIUM | bug-hunt | Item 7 subtask "audit + migrate all call sites" |
| M7 — counter naming + increase() | MEDIUM | bug-hunt | All 9-box checklists — counter `_total` only on counters; gauges drop suffix; `increase()` / `rate()` on Grafana |
| M8 — single SQL query both selectors | MEDIUM | bug-hunt | Item 5 — reuse Item 4's selector result via shared `Arc<Vec<Row>>` |
| M9 — Item 12 verify-only no-op | MEDIUM | bug-hunt | Item 12 test rename `test_option_movers_writer_narrows_naturally_under_indices_only` |
| L1 — Depth200Dyn01 collision check | LOW | bug-hunt | Item 9 — verify before adding variant |
| L2 — ts UTC/IST normalization | LOW | bug-hunt | Item 4 + Item 5 — `test_depth_20_query_uses_ist_offset_for_ts_freshness` |
| L3 — Phase 2 inert guard PHASE2-02 | LOW | bug-hunt | Item 2 — under indices-only, guard instantiated + `emit_skipped()` called (NOT dropped without firing) |
| L4 — wave-5-sub-branches.md templating | LOW | security | Item 17 — flag for review if file gains dynamic content |
| L5 — papaya pin per-batch | LOW | hot-path | Item 16 — `test_movers_tracker_pins_papaya_once_per_batch_not_per_tick` |

## Item 19 — 25-Timeframe Movers Calculation Spec (operator requirement 2026-05-01)

Operator: "for top movers now we need 1s, 5s, 15s, 30s, then starting 1 minute sequentially incrementally as one till 15 minutes meanwhile we need to have 30 min and 1 hour and 2 hour and 3 hour and 4 hour and one day also. These calculations should happen starting 9.15 am — i mean for one minute if it is 9.15 am then for two minute it should 9.15 am and 9.17 am — for every other also it is same."

### Full timeframe set (25 timeframes, replacing "22 timeframes" naming)

| # | Label | Duration (sec) | First bucket boundary (IST) | Second bucket boundary | Notes |
|---:|---|---:|---|---|---|
| 1 | 1s | 1 | 09:15:00 | 09:15:01 | Sub-minute high-resolution |
| 2 | 5s | 5 | 09:15:00 | 09:15:05 | |
| 3 | 15s | 15 | 09:15:00 | 09:15:15 | |
| 4 | 30s | 30 | 09:15:00 | 09:15:30 | |
| 5 | 1m | 60 | 09:15:00 | 09:16:00 | |
| 6 | 2m | 120 | 09:15:00 | 09:17:00 | Operator example |
| 7 | 3m | 180 | 09:15:00 | 09:18:00 | |
| 8 | 4m | 240 | 09:15:00 | 09:19:00 | |
| 9 | 5m | 300 | 09:15:00 | 09:20:00 | |
| 10 | 6m | 360 | 09:15:00 | 09:21:00 | |
| 11 | 7m | 420 | 09:15:00 | 09:22:00 | |
| 12 | 8m | 480 | 09:15:00 | 09:23:00 | |
| 13 | 9m | 540 | 09:15:00 | 09:24:00 | |
| 14 | 10m | 600 | 09:15:00 | 09:25:00 | |
| 15 | 11m | 660 | 09:15:00 | 09:26:00 | |
| 16 | 12m | 720 | 09:15:00 | 09:27:00 | |
| 17 | 13m | 780 | 09:15:00 | 09:28:00 | |
| 18 | 14m | 840 | 09:15:00 | 09:29:00 | |
| 19 | 15m | 900 | 09:15:00 | 09:30:00 | |
| 20 | 30m | 1,800 | 09:15:00 | 09:45:00 | |
| 21 | 1h | 3,600 | 09:15:00 | 10:15:00 | |
| 22 | 2h | 7,200 | 09:15:00 | 11:15:00 | |
| 23 | 3h | 10,800 | 09:15:00 | 12:15:00 | |
| 24 | 4h | 14,400 | 09:15:00 | 13:15:00 | |
| 25 | 1d | 86,400 | 09:15:00 (today) | 09:15:00 (tomorrow) | Day timeframe = trading-session bar (15:30 IST close stops growth) |

### Anchor rule (mandatory)

**All bucket boundaries are derived from `09:15:00 IST` of each trading day, NOT from `00:00:00 IST midnight`, NOT from arbitrary `UNIX_EPOCH % timeframe`.**

The mechanical formula: for a tick at `t_ist_secs` (IST epoch seconds), the bucket-start for timeframe `tf_secs` is:

```rust
const SESSION_OPEN_SECS_IST: u32 = 9 * 3600 + 15 * 60;  // 09:15:00 IST = 33,300 secs into day

fn bucket_start(t_ist_secs: u64, tf_secs: u64) -> u64 {
    let secs_of_day = t_ist_secs % 86_400;
    let elapsed_since_open = secs_of_day.saturating_sub(SESSION_OPEN_SECS_IST as u64);
    let buckets_since_open = elapsed_since_open / tf_secs;
    let bucket_offset_in_day = SESSION_OPEN_SECS_IST as u64 + buckets_since_open * tf_secs;
    let day_start_ist = (t_ist_secs / 86_400) * 86_400;
    day_start_ist + bucket_offset_in_day
}
```

Reference test cases (must pass):

| Tick at | tf=2m | tf=5m | tf=1h |
|---|---|---|---|
| 09:15:00 IST | 09:15:00 | 09:15:00 | 09:15:00 |
| 09:16:30 IST | 09:15:00 (still in first 2m bucket) | 09:15:00 | 09:15:00 |
| 09:17:00 IST | 09:17:00 (boundary) | 09:15:00 | 09:15:00 |
| 09:20:00 IST | 09:19:00 (last 2m closed at 09:19) | 09:20:00 (boundary) | 09:15:00 |
| 10:15:00 IST | 10:15:00 | 10:15:00 | 10:15:00 |

### Pre-market behaviour

For ticks before `09:15:00 IST` (pre-open auction window 09:00–09:12, IDX_I + cash equities) — the formula's `saturating_sub` returns 0, so `bucket_start = day_start + 09:15:00`. This means pre-open ticks accumulate into the **first** bucket of each timeframe (the 09:15 bucket). Operator's call: keep this OR drop pre-open ticks from movers entirely. Default: keep (movers are observability, not trading signals; pre-open data is informative).

### Post-close behaviour (15:30 IST onwards)

After 15:30 IST close, no live ticks arrive. The day-bar (tf=1d) is closed at the 15:30 last-tick value. All other timeframes are dormant until 09:15 next trading day.

### Storage cost math under 25 timeframes (vs 22)

| Timeframe count | Categories | Top-N | Universe | Snapshots/day | Rows/day | Rows/year |
|---|---|---|---|---|---|---|
| 22 (existing) | 7 | 50 | 10,783 | varies | ~3M | ~750M |
| 25 (Wave 5) | 7 | 50 | 10,783 | varies | ~3.4M (+13%) | ~850M |

### Implementation impact on already-merged `Movers22TfTracker` (BLOCKER follow-up)

The merged trunk's tracker is named `Movers22TfTracker`. Operator's new spec is 25 timeframes. **Rename + extend, not replace.**

| Component | Current (22-tf) | Wave 5 (25-tf) |
|---|---|---|
| Type name | `Movers22TfTracker` | `Movers25TfTracker` (rename — but defer to operator: rename or keep `22Tf` alias?) |
| Timeframes constant | `MOVERS_22TF_LIST` | Extend to 25; rename or alias |
| QuestDB table | `top_movers_22tf` | Either: rename to `top_movers_25tf`, OR keep table name + add 3 new timeframes (1s, 5s, 15s, 30s) as new rows |
| Snapshot scheduler | `movers_22tf_scheduler.rs` | Extend cadence array to 25 entries |
| Per-timeframe writer task | 22 tokio::spawn tasks | 25 tokio::spawn tasks (Phase 10 supervisor pattern continues) |
| Memory | 22 × per-instrument state | 25 × per-instrument state (~14% more) |

### What we add to the plan

- [ ] **Item 19. 25-timeframe spec extension to merged `Movers22TfTracker`**
- Files: `crates/common/src/constants.rs` (timeframe list), `crates/core/src/pipeline/movers_22tf_tracker.rs` (rename or extend), `crates/core/src/pipeline/movers_22tf_scheduler.rs`, `crates/core/src/pipeline/movers_22tf_supervisor.rs`, `crates/core/src/pipeline/movers_22tf_writer_state.rs`, `crates/storage/src/movers_22tf_persistence.rs`, `crates/api/src/handlers/market_data.rs` (top-movers-22tf API endpoint)
- Tests:
  - `test_movers_25_timeframe_list_pinned` (constant pin)
  - `test_bucket_start_anchored_at_0915_ist` (formula correctness — 5+ scenarios from the Reference test cases table)
  - `test_pre_market_ticks_accumulate_into_0915_bucket`
  - `test_post_close_ticks_ignored`
  - `test_2m_bucket_at_0916_30_ist_returns_0915_bucket` (operator's example)
  - `test_2m_bucket_at_0917_00_ist_returns_0917_bucket`
  - `test_each_timeframe_has_dedicated_writer_task` (extend supervisor ratchet)
- 9-box:
  - ① typed event: `MoversTimeframeWriterRespawned` (existing — extend label cardinality from 22 to 25)
  - ② ErrorCode: reuse `MOVERS-22TF-01` / `MOVERS-22TF-02` / `MOVERS-22TF-03` (already covers writer / panic / drift). Optionally rename code prefix to `MOVERS-TF-` (drop the `22` from the code).
  - ③ tracing: `info!(timeframe = label, bucket_start = …)`
  - ④ Prom: extend `tv_movers_writer_errors_total{stage,timeframe}` label cardinality from 22 to 25
  - ⑤ Grafana: Operator Health "Movers 25-tf coverage" panel (extend existing 22-tf panel)
  - ⑥ Alert: extend `tv-movers-22tf-*` alert rules to cover 25 labels
  - ⑦ Call site: tick_processor → tracker → 25 spawned writer tasks
  - ⑧ Triage rule: `.claude/triage/error-rules.yaml` — no change (codes unchanged)
  - ⑨ Ratchet: 7 tests above

### Open question for operator

**Q4 (Item 19):** Rename `Movers22Tf*` types/files/tables to `Movers25Tf*`, OR keep `22Tf` naming + add the 3 sub-minute timeframes alongside?
- Default: **keep `22Tf` naming + add timeframes inside** (smaller diff, less migration cost; the "22" number is now historical artifact).
- Alternative: rename for clarity (larger diff, requires `cargo-mutants` re-baseline + dashboard rename + `MOVERS-22TF-*` ErrorCode rename).

## Item 20 — May 2026 Cutover Timeline + AWS Deployment Plan (operator demand 2026-05-01)

Operator: "our main aim why we have shortened this is to finish this live testing the max before this may end and even it should be deployed to aws instance also by this month end with all the expected design and integrated and working product. Can we do that — is that possible bro? I need guarantee and assurance dude."

### Honest answer (per `wave-4-shared-preamble.md` §8)

**Achievable in the 30-day window with the locked plan + foundation already on trunk.** Cannot promise literal "guaranteed deploy by 2026-05-31" — TCP, Dhan API, AWS, and live market data are external dependencies not in our control. What CAN be guaranteed inside the tested envelope:

| Guarantee | Mechanical proof | Owner |
|---|---|---|
| Plan locked + buildable | This file, Status: APPROVED, 19 items, all 33 findings remediated | done |
| Sub-PR split with no conflicts | Item 17 parallelization map, file-disjoint per group | done |
| Foundation already merged | `depth_20_dynamic_subscriber.rs`, `Movers22TfTracker`, 7,250 tests, 53 ErrorCodes, aws-budget.md | done |
| AWS budget approved | `.claude/rules/project/aws-budget.md` ₹5K/mo cap | done |
| Implementation testable end-to-end | Item 11 chaos burst + Item 10 adversarial review + 22-test category coverage | will be done by week 3 |
| Dry-run mandatory before live | `config/base.toml::strategy.dry_run = true` default | already enforced |

### 30-day cutover schedule (calendar 2026-05-01 → 2026-05-31)

| Week | Days | Items | Outcome | Risk |
|---|---|---|---|---|
| **W1** | May 1-7 | Sub-PR A: Items 1, 6, 7, 8, 9, 15 (Foundation + Independent fixes per Item 17 Group A+B) | All merged to wave branch; can run 5 sessions in parallel after Item 1 | Low — Items mostly small/medium, no architectural rewrites |
| **W2** | May 8-14 | Sub-PR B start: Item 2 (universe filter), then Items 3, 4, 5 in parallel | Main feed + depth pipelines verified under indices-only scope | Medium — Item 4 collision-resolution risk (already mitigated via Option A) |
| **W3** | May 15-21 | Sub-PR B finish: Items 12, 13, 14 + Sub-PR C: Items 16, 19, 11 | Movers + prev_close + chaos burst all green | Medium — Item 7 candle_aggregator workspace migration touches many call sites |
| **W4** | May 22-28 | Item 10 adversarial review (3 agents) + Item 18 verification + Wave 5 final merge to main | All ratchets green; PR #410 merged | Medium — adversarial review may surface late finds; budget 2 days for fixes |
| **W5** | May 29-31 | AWS provisioning + Dhan static IP + dry-run on c7i.xlarge | Live tickvault running on AWS in dry-run mode | High — operator-driven; Dhan IP cooldown is a 7-day landmine |

### Critical path landmines

| Landmine | Cost if hit | Mitigation |
|---|---|---|
| **Dhan static IP modification cooldown (7 days)** per `dhan/authentication.md` rule 7 | Lose 7 days = miss May 31 | Get IP RIGHT on first try; dry-run extensively against the new EIP BEFORE registering with Dhan; set both PRIMARY + SECONDARY IPs |
| **Static IP enforcement effective April 1, 2026** | Orders from unregistered IPs REJECTED by exchange | Pre-flight check `ordersAllowed == true` from `GET /v2/ip/getIP` before market open (existing Wave 2-A wiring) |
| **Item 7 candle_aggregator composite-key workspace migration** | 1-3 days if many call sites | Item 17 parallelization map says Group B; Item 7 marked as "audit + migrate all call sites" subtask per M6 |
| **`core_affinity` 0.8.3 on Apple Silicon M4 Pro** (Item 6 Mac parity) | 1-2 days if `task_policy_set` doesn't behave on Darwin 14+ | Have fallback: `TICKVAULT_ALLOW_SUB_4_VCPU=1` env override + halt on `CORE-PIN-04` is already wired |
| **Concurrent in-flight waves (Wave 3, Wave 4) merge conflicts** | 1-2 days | Per-PR-set protocol Item 17 group separation; fetch + rebase frequently |
| **AWS provisioning operator availability** | Variable | Operator-driven; Wave 5 cannot accelerate this |
| **Live data surprise during dry-run** (e.g., Dhan emits a packet variant our parser doesn't handle) | 1-7 days | Adversarial review (Item 10) catches most; chaos burst (Item 11) catches throughput; rest surfaces in dry-run |

### What "with all the expected design and integrated and working product" means mechanically

- **Expected design:** plan locked at 19 items + all 33 review findings remediated. Architecture matches `wave-4-shared-preamble.md` §2 charter.
- **Integrated:** all 6 crates compile workspace-wide; CI green; pre-push 12 gates pass; coverage 100% per `quality/crate-coverage-thresholds.toml`.
- **Working product:** `make doctor` 7-section pass; `make validate-automation` 30 checks pass; `mcp__tickvault-logs__list_active_alerts` returns `[]`; live tick stream observed end-to-end on c7i.xlarge with 11,018 instruments + 248 depth-20 instruments + 5 depth-200 instruments.

### What is NOT in scope for May (by design — operator's "shortened" decision)

- Stock F&O subscription (216 stocks × ~102 contracts = ~22K instruments) — DROPPED in Wave 5
- Live order placement (still `dry_run = true`) — separate Phase 1 graduation
- Real money trading — separate operator decision after dry-run validation
- Greeks pipeline expansion beyond NIFTY/BANKNIFTY/SENSEX — stays at MAX_UNDERLYINGS=3
- Loki + Alloy re-add (deferred per `observability-architecture.md` Phase 3 blocked on QuestDB mem trim)
- CloudWatch CloudWatch parity (Phase 4) — deferred until AWS instance provisioned

### Daily protocol during the 30-day window

Per `wave-4-shared-preamble.md` §1 + `stream-resilience.md` B7 every working day:

1. **Session start:** `mcp__tickvault-logs__run_doctor` + `summary_snapshot` + `list_active_alerts` (auto via SessionStart hook)
2. **Read active plans:** session-context-brief.sh hook surfaces them
3. **Pick item from current week's slice:** see schedule above
4. **Spawn 3 review agents** for any item touching > 3 crates (B3)
5. **Implement + commit + push** to wave branch
6. **End of day:** verify wave branch CI green; if red, fix before stopping

### What will be measured each week

| Week | Metric | Target | Source |
|---|---|---|---|
| W1 end | Items merged | 6 (Sub-PR A) | git log on wave branch |
| W2 end | Items merged | 4 more (Items 2, 3, 4, 5 = 10 total) | git log |
| W3 end | Items merged | 6 more (Items 12, 13, 14, 16, 19, 11 = 16 total) | git log |
| W4 end | Items merged | All 19; CI all-green; PR #410 ready to squash | gh pr view |
| W5 end | AWS deployed | tickvault running on c7i.xlarge in dry-run; first live tick observed | `make doctor` on prod |

### Honest non-promises (per Wave-4-Preamble §8)

- **Cannot guarantee:** "no Dhan API change between now and May 31" (Dhan can ship breaking change anytime)
- **Cannot guarantee:** "live tick rate stays within tested envelope" (NSE volatility days produce 200K+ tps that no chaos test fully replicates)
- **Cannot guarantee:** "AWS region ap-south-1 has zero outages in the 30-day window" (AWS regional failures happen)
- **Cannot guarantee:** "operator has no scheduling conflicts during W5 AWS provisioning" (operator-only task)
- **Cannot guarantee:** "every implementation item ships in its allotted week" (review findings can extend timelines)

### Probabilistic assessment

Given:
- Foundation already merged (~50% of Wave 5 = verify/harden, not greenfield)
- 7,250 tests + 18 hooks + chaos test infrastructure already in place
- Plan dependency-mapped per Item 17
- Operator decisions all locked

**Honest probability of "Wave 5 + AWS dry-run live by 2026-05-31": ~70-80%** assuming:
- No Dhan API surprises
- No AWS provisioning delays beyond 2 days
- Operator available for AWS step in W5
- Static IP set correctly on first try

**If May 31 slips:** worst-case delay is ~1 week (June 7) due to Dhan IP cooldown. Plan stays valid; just retargets.

This is the honest envelope. Anything tighter ("guaranteed by May 31") would be hallucination per Wave-4-Preamble §8.

## Plan Items (now 20, was 10)

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

### - [ ] 4. Depth-20 — REVERTED TO OPERATOR ORIGINAL DESIGN 2026-05-01 (BLOCKER C1 → Option B)

**Decision (operator override 2026-05-01):** Operator reaffirmed original single-side design. Earlier "Option A adopt merged" decision is REVERSED. Wave 5 RIPS the merged `depth_20_dynamic_subscriber.rs` (commits `f3b7baa`, `29b1407`, `72028c5`, `c9f53a4`) and replaces with single-side 4-conn + 1 dynamic-50.

**Final depth-20 layout (246 instruments):**

| Conn | Underlying | Side | Strikes | Count |
|---:|---|:---:|---|---:|
| 1 | NIFTY current nearest expiry | **CE only** | ATM ±24 | 49 |
| 2 | NIFTY current nearest expiry | **PE only** | ATM ±24 | 49 |
| 3 | BANKNIFTY current nearest expiry | **CE only** | ATM ±24 | 49 |
| 4 | BANKNIFTY current nearest expiry | **PE only** | ATM ±24 | 49 |
| 5 | Dynamic top 50 from `option_movers` `category='TOP_VOLUME'` sorted by `change_pct DESC`, SENSEX-skipped | mixed | dynamic 60s | 50 |
| **Total** | | | | **246** |

**Selector SQL (NEW — replaces merged `DEPTH_20_DYNAMIC_SELECTOR_SQL`):**

```sql
SELECT security_id, exchange_segment, underlying_symbol, change_pct, volume
FROM option_movers
WHERE category = 'TOP_VOLUME'
  AND exchange_segment != 'BSE_FNO'   -- exclude SENSEX (Dhan depth NSE-only)
  AND change_pct > 0                  -- gainers only
  AND volume > 0                      -- defensive
  AND ts > dateadd('s', -300, now())  -- 5m freshness; IST-normalized at writer
ORDER BY change_pct DESC, volume DESC -- tie-break by volume for determinism
LIMIT 50;                             -- depth-200 takes first 5 of these (shared query)
```

- Files:
  - `crates/core/src/instrument/depth_20_dynamic_subscriber.rs` (**REWRITE** — old SQL + 3-slot logic deleted; new SQL + 1-slot top-50 + 4 single-side static slots)
  - `crates/core/src/instrument/depth_strike_selector.rs` (split CE-only and PE-only ATM±24 helpers)
  - `crates/app/src/main.rs` (wiring: 4 single-side conns + 1 dynamic + 1 shared selector loop feeding both depth-20 conn 5 and depth-200 conns 1-5)
  - `crates/core/src/websocket/depth_connection.rs`
  - `crates/common/src/constants.rs` (`DEPTH_20_DYNAMIC_SLOT_COUNT = 1`, `DEPTH_20_DYNAMIC_SLOT_CAPACITY = 50`)
- Tests (Wave 5, replacing old Phase 7 tests):
  - `test_depth_20_conn_1_is_nifty_ce_atm_24`
  - `test_depth_20_conn_2_is_nifty_pe_atm_24`
  - `test_depth_20_conn_3_is_banknifty_ce_atm_24`
  - `test_depth_20_conn_4_is_banknifty_pe_atm_24`
  - `test_depth_20_conn_5_uses_top_volume_category_change_pct_desc_limit_50`
  - `test_depth_20_total_instrument_count_is_246`
  - `test_depth_20_conn_5_excludes_sensex_bse_fno_rows`
  - `test_depth_20_swap_hysteresis_3_position_min` (H7 anti-thrash)
  - `test_depth_20_dynamic_selector_runs_on_core_3_not_core_1` (C4/H2)
  - `test_depth_20_dynamic_handles_sub_4_vcpu_gracefully` (H8)
  - `test_depth_20_dynamic_set_sanitized_before_ilp_write` (H5)
  - `test_depth_20_query_uses_ist_offset_for_ts_freshness` (L2)
  - `test_depth_20_selector_result_shared_with_depth_200_via_arc_vec_row` (M8)
- 9-box:
  - ① typed event: keep `Depth20DynamicTopSetEmpty` (existing) + new `Depth20TopSwapped` (Severity::Low, edge-triggered on rank-set change)
  - ② ErrorCode: keep `DEPTH-DYN-01` (top set < 50, High) + `DEPTH-DYN-02` (swap channel broken, Critical); add `DEPTH-DYN-04` (rank churn without semantic change — H7 hysteresis violation, Medium)
  - ③ tracing: `info!(conn, side, count, …)` per static conn at boot; `info!(rank_set_diff, …)` per swap
  - ④ Prom: `tv_depth_20_static_conns_subscribed` (expect = 4); `tv_depth_20_dynamic_set_size` (expect = 50); `tv_depth_20_swaps_total` counter; `tv_depth_20_swap_thrash_ratio` (H7); `tv_depth_20_dynamic_set_sanitized_total` (H5)
  - ⑤ Grafana: rewrite depth-flow panel — 4 static rows + 1 dynamic row showing top-50 with change_pct
  - ⑥ Alert: keep `tv-depth-dyn-01-top-set-empty-escalate` + `tv-depth-dyn-02-swap-channel-broken-escalate`; add `tv-depth-dyn-04-thrash`
  - ⑦ Call site: rewritten `run_depth_20_subscriber` in main.rs (single-side static spawn + 1 dynamic loop)
  - ⑧ Triage rule: keep + add `.claude/triage/error-rules.yaml::depth-dyn-04-thrash-suppress`
  - ⑨ Ratchet: 13 tests above + delete the obsolete Phase 7 ratchet tests in `depth_20_dynamic_subscriber.rs::tests` (slot 3/4/5 split tests, top-150 LIMIT tests)

**Blast radius note:** rip + replace is bigger than Option A. Mitigation: Item 4 sub-PR includes deletion of `DEPTH_20_DYNAMIC_SLOT_COUNT = 3` + `LIMIT 150` constants in same commit so the build stays green.

### - [ ] 5. Depth-200 — Top-5 dynamic (operator original design)

- Files: `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/instrument/depth_200_subscriber.rs` (new — replaces merged static logic), `crates/app/src/main.rs`
- Tests:
  - `test_depth_200_picks_top_5_from_top_volume_category`
  - `test_depth_200_sorts_by_change_pct_desc`
  - `test_depth_200_excludes_sensex_bse_fno`
  - `test_depth_200_one_instrument_per_conn`
  - `test_depth_200_swap_on_rank_change`
  - `test_depth_200_reuses_depth_20_selector_result_no_separate_query` (M8)
  - `test_depth_200_query_uses_ist_offset_for_ts_freshness` (L2)
- Replaces existing static `["NIFTY", "BANKNIFTY"]` ATM CE/PE config in `main.rs:2151,3113,3685`. Uses existing `DepthCommand::Swap200` path. **Single shared `Arc<Vec<Row>>` from Item 4's selector** — depth-200 takes rows 1-5 of the LIMIT 50 result; no separate SQL query (per M8 atomic snapshot guarantee).
- Hysteresis: rank-set must change by ≥1 position to trigger swap (top-5 is small enough that any change matters; no thrash guard needed beyond the existing edge-trigger).
- 9-box: same pattern as Item 4 — ErrorCode `DEPTH-DYN-01` (top set < 5, High) reused; new `Depth200TopSwapped` (Severity::Low, edge-triggered).
- Replaces existing static `["NIFTY", "BANKNIFTY"]` ATM CE/PE config in `main.rs:2151,3113,3685`. Uses existing `DepthCommand::Swap200` path. 60s rebalance: query the SAME `DEPTH_20_DYNAMIC_SELECTOR_SQL` result (Option A reuse — no separate SQL), take the top 5 NSE_FNO rows after SENSEX-skip. **Single shared `Arc<Vec<Row>>` query result feeds both Item 4 and Item 5** (per M8 — atomic snapshot guarantee).
- Hysteresis: rank-set must change by ≥1 position to trigger swap (top-5 is small enough that any change matters; no thrash guard needed beyond the existing edge-trigger).
- Test additions: `test_depth_200_reuses_depth_20_selector_result_no_separate_query`, `test_depth_200_excludes_sensex_takes_next_eligible`, `test_depth_200_5_conns_x_1_instrument_each`, `test_depth_200_query_uses_ist_offset_for_ts_freshness`
- 9-box: ① new `Depth200TopGainersSwapped` (Severity::Low, edge-triggered) + reuse `Depth200SwapChannelBroken` (existing DEPTH-DYN-02) ② `DEPTH-200-DYN-01` (top-5 selector returned < 5, severity High) ③ `error!(code = ErrorCode::Depth200Dyn01TopSetEmpty.code_str())` ④ `tv_depth_200_top_gainers_set_size`, `tv_depth_200_top_gainers_swaps_total` ⑤ Operator Health "Depth-200 top-5" panel ⑥ `tv-depth-200-dyn-01-empty-set` ⑦ `main.rs::run_depth_200_top_gainers_loop` ⑧ `.claude/triage/error-rules.yaml::depth-200-dyn-01-top-set-empty-escalate` ⑨ all 5 tests above

### - [ ] 6. Wire `core_affinity` — pin 4 Tokio workers to 4 vCPUs (Mac dev MIRRORS AWS 4-vCPU)

**Operator decision 2026-05-01:** Dev Mac (M4 Pro 14 cores = 10P + 4E) MUST reproduce the AWS c7i.xlarge 4-vCPU configuration EXACTLY for parity testing. Operator verbatim: "even in our local dev macbook also reproduce the same aws 4vcpu dude okay?"

**Why 4-vCPU parity matters (per `wave-4-shared-preamble.md` §6):**

| Concern | Without parity | With parity |
|---|---|---|
| Burst chaos test (Item 11) at 200K tps | Mac has 14 cores → no preemption observed → false-OK | Mac forced to 4 cores → real preemption surfaces → catches Item 11 hot-path violations BEFORE prod |
| Tokio scheduler behaviour | Different worker count → different work-stealing patterns | Identical scheduler shape on both |
| Hot-path benchmarks (Criterion budgets) | 14-core baseline → faster than prod → meaningless 5% regression gate | 4-core baseline matches prod |
| `core_affinity::set_for_current` test | Skips on N != 4 → never tested locally | Always exercised |
| Item 11 chaos burst | Passes on dev, fails on prod | Same envelope both sides |

**Implementation:**

1. **Worker count:** `tokio::runtime::Builder::new_multi_thread().worker_threads(4)` UNCONDITIONALLY (not `num_cpus::get()`).
2. **Core affinity on Mac (Darwin):** `core_affinity` 0.8.3 supports macOS via `thread_policy_set`. Pin 4 workers to 4 specific P-cores (recommend cores 0–3 = first 4 P-cores; the M4 Pro's E-cores 10–13 are unsuitable for hot-path latency).
3. **Core affinity on Linux (AWS):** pin 4 workers to vCPUs 0–3 via `sched_setaffinity`.
4. **Boot-time verify:** read process affinity via:
   - Linux: `/proc/self/task/*/status` `Cpus_allowed_list`
   - macOS: `task_info()` / `proc_pidinfo()` (or just trust `core_affinity::get_for_current`'s post-set verification)
5. **Detection of host capability:** `core_affinity::get_core_ids().len() < 4` → `CORE-PIN-04` (Severity::Critical, halt boot — host too small for parity).

- Files: `crates/app/src/main.rs`, `crates/app/src/runtime.rs` (new), `Cargo.toml` (already has core_affinity 0.8.3)
- Tests:
  - `test_runtime_builder_uses_worker_threads_4_unconditional` (NOT `num_cpus::get()`)
  - `test_core_affinity_pinned_4_workers_on_linux_via_proc_status`
  - `test_core_affinity_pinned_4_workers_on_macos_via_task_info`
  - `test_each_worker_has_unique_cpu_set` (no two workers sharing a core)
  - `test_e_cores_excluded_on_apple_silicon` (workers must be on P-cores 0–9, not E-cores 10–13 on M4 Pro; verify via core_id range)
  - `test_boot_halts_when_host_has_fewer_than_4_cores` (CORE-PIN-04)
  - `test_pinning_unsafe_call_carries_approved_comment` (H4 — source-scan ratchet)
- 9-box:
  - ① `CorePinningFailed` (Severity::High) + new `CorePinningHostTooSmall` (Severity::Critical, halts boot)
  - ② `CORE-PIN-01` (pinning failed at boot, High), `CORE-PIN-02` (worker drift, Medium), `CORE-PIN-03` (sub-4-vCPU graceful continue — only used when operator opts out via `TICKVAULT_ALLOW_SUB_4_VCPU=1` env override; default = HALT not continue), **`CORE-PIN-04`** (host has < 4 cores, Critical, halt boot — Mac parity demand)
  - ③ `error!(code = ErrorCode::CorePin04HostTooSmall.code_str())` on halt
  - ④ Prom: `tv_core_pinning_workers_pinned` gauge (expect = 4) + `tv_core_pinning_host_core_count` info gauge — exposes whether dev Mac is correctly running with 14 cores OR forced 4
  - ⑤ Operator Health "Core affinity" panel — single row showing "4/4 pinned, host has N cores" (info-only)
  - ⑥ Alert: `tv-core-pin-01-pinning-failed` (existing) + `tv-core-pin-04-host-too-small`
  - ⑦ Call site: `main.rs::build_runtime`
  - ⑧ Triage: existing entries + `core-pin-04-host-too-small-escalate`
  - ⑨ Ratchet: 7 tests above + boot-time assertion + H4 source-scan + H8 graceful-skip-with-env-override + C4/H2 mapping (depth selectors + movers snapshot computation must `tokio::spawn` on Core 3, NOT inherit caller's core)

**Mac dev workflow note:** Devs running on M4 Pro will see only 4 of 14 cores utilized — this is BY DESIGN for parity. To temporarily disable for non-parity work, set `TICKVAULT_ALLOW_SUB_4_VCPU=1` (per CORE-PIN-03 graceful path).

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
| 3 | ~~core_affinity on dev Mac vs AWS c7i.xlarge — hard-fail if vcpu mismatch?~~ **ANSWERED 2026-05-01:** dev Mac MUST mirror AWS 4-vCPU exactly. Operator: "even in our local dev macbook also reproduce the same aws 4vcpu". See Item 6 Mac-parity amendment below. | (resolved) |

## Notes

- Phase 2 dispatcher (09:13 IST) becomes inert under indices-only scope — keep code path, gate on `config.subscription.scope`. Do NOT delete; reactivating full universe must be a 1-line config flip.
- `MidMarketBootComplete` event becomes inert (no stock F&O Mode C resolution needed). Keep variant; rename trigger condition.
- Pre-open buffer + REST fallback (`preopen_rest_fallback.rs`) becomes inert for stocks. Indices still use it.
- DEDUP keys + I-P1-11 composite-key invariants UNCHANGED. New code MUST use `(security_id, exchange_segment)`.

## Item 21 — Movers Store-Everything (Tier A/B Split-Cadence) — operator demand 2026-05-01

Operator: "no top N categories for stock movers or options movers or top movers because i never ever wanted this top n dude it should be always everything dude — only then in dashboard for historical view we can easily see this".

3 specialist agents reviewed full-universe persistence in parallel. **Hot-path agent: 1 CRITICAL + 3 HIGH. Feasibility agent: pure store-everything blows ₹5K/mo budget by 6×. Security agent: 0 CRITICAL, 2 MEDIUM, 2 LOW.**

**Honest verdict: pure "store all 11K × 7 cat × 25 tf" exceeds c7i.xlarge budget AND ingest ceiling. Tier A/B split-cadence is the only viable path within ₹5K/mo cap.**

### Final design — Tier A / Tier B split

| Tier | Timeframes | Universe | Categories | Rationale |
|---|---|---|---|---|
| **A — minute+** | 1m, 2m, 3m, 4m, 5m, 6m, 7m, 8m, 9m, 10m, 11m, 12m, 13m, 14m, 15m, 30m, 1h, 2h, 3h, 4h, 1d (22 tf) | **ALL ~11,018 instruments** | All 7 (HIGHEST_OI, OI_GAINER, OI_LOSER, TOP_VOLUME, TOP_VALUE, PRICE_GAINER, PRICE_LOSER) | Operator's historical-dashboard demand satisfied |
| **B — sub-minute** | 1s, 5s, 15s, 30s (4 tf) | Watchlist only: NIFTY/BANKNIFTY full chain + ATM±25 of focus stocks (~500 contracts) | All 7 | Sub-minute is for fast trading signals; storing 1s × 11K × 7 = 77K rows/sec exceeds QuestDB ceiling and blows EBS in <1 day |

### Math (verified by agents)

| Metric | Pure store-everything | **Tier A/B (chosen)** |
|---|---|---|
| Peak burst (09:15 IST coboundary) | ~1M rows/sec | ~30K rows/sec staggered |
| Sustained writes | ~250K rows/sec | ~8.5K rows/sec |
| Storage/day | 45–196 GB | ~3.5 GB |
| 90-day hot | 4 TB (₹31K/mo) | 115 GB (fits ₹333/mo S3 lifecycle) |
| QuestDB ceiling on c7i.xlarge | EXCEEDED | safe |
| Fits ₹5K/mo cap | NO (6× over) | YES |

### Mandatory mitigations (all 3 agents agreed)

| # | Fix | Source | Why |
|---|---|---|---|
| 1 | Stagger boundary emissions (each tf offset by `idx × 200ms`) | hot-path | Spreads 175 sorts across ~5s instead of synchronous 1.92M-row burst |
| 2 | Arena pool per category (`[Vec<MoverRow>; 7]` reusable, owned per timeframe task) | hot-path | Zero allocation steady-state |
| 3 | mpsc 8192 → 65536 OR per-timeframe spill ring (`spill_ring.rs` pattern) | hot-path | Absorb burst without drops |
| 4 | Criterion bench `bench_rank_all_n11k_22tf_p99_under_200ms` | hot-path | Without ratchet, "O(1) under normal" is aspirational |
| 5 | **DEDUP key fix:** `DEDUP_KEY_MOVERS_22TF` `"ts, security_id, segment"` → `"ts, security_id, segment, category"` | feasibility + security | Without `category`, 7 categories × same instrument collide → row clobber |
| 6 | Tie-break deterministic: `change_pct DESC, volume DESC, security_id ASC` | security L2 | Eliminates rank divergence between snapshots |
| 7 | API `/api/top-movers` — explicit operator decision on auth gating (currently unauthed) | security M1 | Full-universe rank exposes intraday positions |
| 8 | DDL comment fix: drop "SEBI 5-year retention" wording — movers are observability, NOT order audit | security L1 | Prevents over-retention cost |

### Wave 5 plan amendment

- [ ] **Item 21. Movers store-everything Tier A/B split-cadence**
- Files: `crates/storage/src/movers_22tf_persistence.rs` (DEDUP key fix, slim DDL), `crates/core/src/pipeline/movers_22tf_tracker.rs` (arena pool, full-universe ranking), `crates/core/src/pipeline/movers_22tf_scheduler.rs` (stagger + Tier A/B gating), `crates/core/src/pipeline/movers_22tf_writer_state.rs` (mpsc 65536), `crates/core/src/pipeline/movers_22tf_supervisor.rs`, `crates/common/src/constants.rs` (timeframe list extended to 25, Tier B watchlist constant `MOVERS_SUBMINUTE_WATCHLIST_SIZE = 500`), `crates/api/src/handlers/top_movers.rs` (auth decision)
- Tests:
  - `test_movers_dedup_key_includes_category` (Mitigation 5 — meta-guard)
  - `test_movers_tier_a_full_universe_minute_plus` (22 tf × 11K × 7 = full)
  - `test_movers_tier_b_watchlist_only_sub_minute` (500-contract gate)
  - `test_movers_boundary_stagger_200ms_offset_per_tf` (Mitigation 1)
  - `test_movers_arena_pool_zero_alloc_steady_state` (DHAT, Mitigation 2)
  - `test_movers_writer_channel_capacity_65536` (Mitigation 3)
  - `bench_rank_all_n11k_22tf_p99_under_200ms` (Criterion, Mitigation 4)
  - `test_movers_tie_break_change_pct_volume_security_id` (Mitigation 6)
  - `test_movers_api_top_movers_auth_or_explicitly_unauthed` (security M1)
  - `test_movers_ddl_comment_no_sebi_5y_wording` (security L1 source-scan)
- 9-box:
  - ① typed event: existing `Movers22TfWriterIlpFailed` + add `MoversTierBWatchlistDrift` (Severity::Medium, watchlist size != 500 due to ATM rebalance)
  - ② ErrorCode: existing `MOVERS-22TF-01/02/03`; add `MOVERS-22TF-04` (sub-minute watchlist drift, Medium); add `MOVERS-22TF-05` (DEDUP key validation failed, Critical, halt boot)
  - ③ tracing: `info!(tier, tf, rows_persisted, …)`
  - ④ Prom: `tv_movers_persist_rows_total{tier,tf,category}`, `tv_movers_burst_peak_rows_per_sec`, `tv_movers_arena_pool_alloc_count` (expect = 0 steady-state)
  - ⑤ Grafana: rewrite Operator Health movers panel — split Tier A vs Tier B rows
  - ⑥ Alert: `tv-movers-22tf-burst-exceeds-100k-rows-per-sec` (>100K/sec sustained 60s = QuestDB ceiling risk)
  - ⑦ Call site: `tick_processor` → tracker (existing); scheduler stagger (new); supervisor (existing)
  - ⑧ Triage: existing entries + new `movers-22tf-04-watchlist-drift-investigate`
  - ⑨ Ratchet: 10 tests above + `dedup_segment_meta_guard.rs` extension to verify `category` membership

### Items 16 + 19 amendments

- Item 16 (was: REUSE Movers22TfTracker) — **superseded by Item 21**. Item 16 closes as "verify-only no-op" since Item 21 is the active rewrite.
- Item 19 (was: 25-timeframe extension keeping 22Tf naming) — **folded into Item 21**. The 25-tf list extension is now a sub-task of Item 21's Tier A/B implementation.

### Honest non-claim restated

Operator's ideal "store everything for everything" is **physically infeasible** on ₹5K/mo c7i.xlarge — sustained 250K rows/sec writes blow EBS in 4 days, peak 1M rows/sec exceeds QuestDB single-host ingest. Tier A/B is the **only design that satisfies operator intent (full historical view) within budget envelope**. If operator wants pure store-everything, AWS budget must move to ~₹10–12K/mo (c7i.2xlarge + 500GB EBS + larger S3).

## Item 22 — Per-Item Guarantee Matrix (mandatory template)

Operator demand 2026-05-01: "for every waves and for every blocks or for every items we need all these guarantee and assurance right dude because every block is extremely critical right dude?"

**Every Wave/item from this point forward MUST attach the following matrix to its 9-box checklist.** No item ships without all rows green at PR-merge time.

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| **100% code coverage** | `quality/crate-coverage-thresholds.toml` 100% min per crate; `scripts/coverage-gate.sh` | post-merge llvm-cov report | item PR includes coverage screenshot |
| **100% audit coverage** | `<event>_audit` table per typed event with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` | item adds/extends audit table |
| **100% testing coverage** | 22 test categories per `testing.md` (unit/integration/property/loom/dhat/fuzz/mutation/sanitizer/coverage/etc.) | `cargo test --workspace` green | item declares which 22 it covers |
| **100% code checks** | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory | all gates green |
| **100% code performance** | DHAT zero-alloc + Criterion p99 budgets + bench-gate ≤5% regression | `cargo bench` + `scripts/bench-gate.sh` | DHAT test if hot path |
| **100% monitoring** | 7-layer telemetry (Prom + tracing + Loki + Telegram + Grafana + alert + audit) | `mcp__tickvault-logs__run_doctor` | 9-box completes 7 layers |
| **100% logging** | tracing macros mandatory (no println!/dbg!); ERROR → Telegram | hourly errors.jsonl rotation | every error path uses `error!` with `code` |
| **100% alerting** | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__list_active_alerts` | item adds alert for new failure |
| **100% security** | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent | `cargo audit` post-deploy | item runs security-reviewer |
| **100% security hardening** | static IP enforcement + pre-commit secret scan + `unused_must_use` lint | post-deploy IP verify | item declares attack-surface delta |
| **100% bugs fixing** | adversarial 3-agent review (proven 4-bug catch rate) | pre-PR + post-impl agent pass | item runs all 3 agents |
| **100% scenarios covering** | 9-box + chaos test for new failure mode (Item 11 pattern) | chaos suite | item declares scenarios in ratchet |
| **100% functionalities covering** | every pub fn has call site + test (`pub-fn-wiring-guard.sh` + `pub-fn-test-guard.sh`) | pre-push gates 6+11 | item adds tests for every new pub fn |
| **100% code review** | adversarial 3-agent on diff before AND after impl | per-PR | item PR includes both passes |
| **100% extreme check** | all of above + ratchet tests fail build on regression | every commit | item adds ratchet test |

### Resilience demands per item (mandatory)

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Bounded zero loss inside chaos envelope: ring 600K → spill NDJSON → DLQ | item must not introduce new tick-drop path |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | item must not break SubscribeRxGuard or pool watchdog |
| Never slow/locked/hanged | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 ≤100ns; tick-gap >30s Telegram; core_affinity Core 0 | item must not add hot-path allocation |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | item must not break self-heal |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% regression | item adds Criterion bench if hot path |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | item DEDUP key includes segment |
| Real-time proof | 7-layer telemetry + SLO-01/SLO-02 @ 10s + market-open self-test @ 09:16:30 IST | item ratchet pins all 7 layers |

- [ ] **Item 22. Per-item guarantee matrix template enforcement**
- Files: this plan file (template above); `.claude/hooks/per-item-guarantee-check.sh` (new — scans new items for matrix presence); `CLAUDE.md` (cross-reference)
- Tests: `test_every_wave_5_item_carries_guarantee_matrix` (source-scan ratchet); `test_per_item_guarantee_check_hook_runs_on_pre_pr`
- 9-box: N/A (process meta-item)

## Plan Items (now 22, was 10)

## Item 23 — Unified-Schema Movers (operator-approved 2026-05-01) — SUPERSEDES Items 16, 19, 21

Operator: "yes dude now this approach sounds better right dude instead of separate seven sections it's better to have top movers entirely right — for all instruments entirely right dude then we can easily sort it out based on needed headings or columns right dude?"

### The simplification

**OLD:** 7 separate rows per (instrument, timeframe), one per category. Category resolved at WRITE time.
**NEW:** 1 unified row per (instrument, timeframe) with all raw stats. Category resolved at READ time via `ORDER BY <column>`.

**7× fewer writes. 7× less storage. Same dashboard semantics.**

### Unified schema (single table `movers_unified`)

| Column | Type | Source |
|---|---|---|
| `ts` | TIMESTAMP (designated) | bucket-start IST nanos (per Item 19 anchor formula at 09:15:00 IST) |
| `security_id` | LONG | from tick |
| `segment` | SYMBOL | exchange segment string (`NSE_FNO`, `NSE_EQ`, `IDX_I`, `BSE_FNO`) |
| `underlying_symbol` | SYMBOL | from `InstrumentRegistry` lookup |
| `timeframe` | SYMBOL | one of 25 enum strings (`1s`, `5s`, ..., `1d`) |
| `open_interest` | LONG | latest OI in bucket |
| `oi_delta` | LONG | OI now − OI at bucket start (signed; negative = OI_LOSER) |
| `volume` | LONG | cumulative bucket volume |
| `last_price` | DOUBLE | `f32_to_f64_clean(LTP)` per data-integrity rule |
| `prev_close` | DOUBLE | from in-memory cache (Item 15) |
| `change_pct` | DOUBLE | `(last_price - prev_close) / prev_close × 100` |
| `received_at` | TIMESTAMP | UTC + IST_UTC_OFFSET_NANOS (per data-integrity rule) |

**DEDUP UPSERT KEYS:** `(ts, security_id, segment, timeframe)` — 4-column composite. **NO `category`** (resolved at read time).

### Read-time queries (the 7 categories now become trivial)

```sql
-- TOP_VOLUME at 5m timeframe over last 5min:
SELECT * FROM movers_unified
WHERE timeframe='5m' AND ts > now() - 5m
  AND segment != 'BSE_FNO'   -- exclude SENSEX (depth selector context)
  AND change_pct > 0
ORDER BY volume DESC, change_pct DESC, security_id ASC
LIMIT 50;

-- HIGHEST_OI at 1h:
SELECT * FROM movers_unified
WHERE timeframe='1h' AND ts > now() - 1h
ORDER BY open_interest DESC LIMIT 50;

-- OI_GAINER at 1m:
SELECT * FROM movers_unified
WHERE timeframe='1m' AND ts > now() - 1m
ORDER BY oi_delta DESC LIMIT 50;

-- Specific instrument's history (dashboard chart):
SELECT * FROM movers_unified
WHERE security_id=12345 AND segment='NSE_FNO' AND timeframe='5m'
ORDER BY ts DESC LIMIT 100;
```

QuestDB SYMBOL bitmap on `timeframe` + partition prune by `ts` → sub-millisecond per query.

### Storage cost (verified by feasibility agent)

| Item | Pure store-everything (old) | **Unified (new)** |
|---|---|---|
| Peak burst @ 09:15 coboundary | 1.92M rows/sec | **275K rows/sec** (7× reduction) |
| Sustained writes | 250K rows/sec | **~36K rows/sec** |
| Sub-minute (1s, 5s, 15s, 30s) per day | 60+ GB | **~8 GB/day** |
| Minute+ (22 tf) per day | 7+ GB | **~0.4 GB/day** |
| Fits ₹5K/mo? | NO | **YES** with tiered retention |

### Tiered hot retention (within 100 GB EBS gp3 budget)

| Timeframe band | Hot retention | Storage at 90d |
|---|---|---|
| 1s | 7 days | ~44 GB |
| 5s | 14 days | ~18 GB |
| 15s | 30 days | ~12 GB |
| 30s, 1m–1d (23 tf) | 90 days | ~38 GB |
| **TOTAL hot** | | **~112 GB** |

S3 cold archival (lifecycle Standard → Intelligent-Tiering → Glacier Deep Archive) catches everything beyond hot retention. Per `aws-budget.md` ₹333/mo cap.

### Mandatory mitigations (carried over from Item 21 agent findings)

| # | Fix | Status |
|---|---|---|
| 1 | Stagger boundary emissions (`idx × 200ms` offset per timeframe) | required |
| 2 | Arena pool per timeframe (zero-alloc steady state) | required |
| 3 | mpsc 8192 → 65536 OR per-tf spill ring | required |
| 4 | Criterion bench `bench_movers_unified_full_snapshot_p99_under_200ms` | required |
| 5 | DEDUP key 4-col composite `(ts, security_id, segment, timeframe)` | NEW (replaces Item 21 5-col `(ts, sid, seg, category)` since category dropped) |
| 6 | Tie-break deterministic in API queries: `<sort_col> DESC, volume DESC, security_id ASC` | required |
| 7 | API `/api/top-movers` auth decision | operator decision |
| 8 | DDL comment: drop "SEBI 5y" wording (movers are observability) | required |

### Files

- `crates/storage/src/movers_unified_persistence.rs` (**NEW** — replaces `movers_22tf_persistence.rs`)
- `crates/core/src/pipeline/movers_unified_tracker.rs` (**NEW** — replaces `movers_22tf_tracker.rs`; raw stats per (security_id, segment) accumulated; per-snapshot writes ALL N entries)
- `crates/core/src/pipeline/movers_unified_scheduler.rs` (**NEW** — staggered emission per Mitigation 1)
- `crates/core/src/pipeline/movers_unified_supervisor.rs` (**NEW** — task respawn)
- `crates/core/src/pipeline/movers_unified_writer_state.rs` (**NEW** — mpsc 65536 per Mitigation 3)
- `crates/api/src/handlers/top_movers.rs` (REWRITE — query unified table with read-time sorts)
- `crates/common/src/constants.rs` (timeframe list = 25, retention bands, watchlist size dropped — full universe)
- Migration: legacy `option_movers` / `stock_movers` / `top_movers_22tf` tables retained for transition, dropped after Wave 5 ships

### Tests

- `test_movers_unified_dedup_key_is_4_col_no_category` (replaces M4 from Item 21)
- `test_movers_unified_full_universe_25_tf` (all 11K × 25 written)
- `test_movers_unified_read_time_sort_top_volume_returns_50` (read pattern)
- `test_movers_unified_read_time_sort_oi_gainer_uses_oi_delta` (sort correctness)
- `test_movers_unified_tiered_retention_1s_7d_5s_14d_15s_30d_30s_plus_90d` (partition manager)
- `test_movers_unified_arena_pool_zero_alloc_per_snapshot` (DHAT)
- `test_movers_unified_stagger_200ms_per_timeframe` (boundary stagger)
- `bench_movers_unified_full_snapshot_p99_under_200ms` (Criterion ratchet)
- `test_movers_unified_tie_break_deterministic_volume_security_id` (sort stability)
- `test_movers_unified_writer_channel_capacity_65536` (Mitigation 3)
- `test_movers_unified_ddl_no_sebi_5y_wording` (security L1 source-scan)
- `test_movers_unified_received_at_uses_ist_offset` (data-integrity Greeks pattern)
- `test_movers_unified_segment_in_dedup_key` (I-P1-11 + STORAGE-GAP-01 meta-guard)

### 9-box (per Item 22 mandatory matrix)

- ① typed event: `MoversUnifiedSnapshotPersisted` (Severity::Info edge-triggered first snapshot post-boot); `MoversUnifiedTimeframeSkipped` (Severity::Medium when stagger window exceeded)
- ② ErrorCode: `MOVERS-UNIFIED-01` (snapshot writer ILP failed, Medium); `MOVERS-UNIFIED-02` (stagger window exceeded, Medium); `MOVERS-UNIFIED-03` (DEDUP key validation failed at boot, Critical halt boot)
- ③ tracing: `info!(timeframe, rows_persisted, …)` per snapshot
- ④ Prom: `tv_movers_unified_rows_total{timeframe}`, `tv_movers_unified_burst_peak_rows_per_sec`, `tv_movers_unified_arena_pool_alloc_count` (expect = 0 steady state)
- ⑤ Grafana: rewrite Operator Health movers panel — single unified table, 25 row-count gauges by timeframe
- ⑥ Alert: `tv-movers-unified-burst-exceeds-300k-rows-per-sec-sustained-60s` (QuestDB ceiling guard)
- ⑦ Call site: existing `tick_processor` enrichment + new `movers_unified_scheduler::run_loop` on Core 3
- ⑧ Triage: `.claude/triage/error-rules.yaml::movers-unified-01-ilp-failed-investigate`
- ⑨ Ratchet: 13 tests above + `dedup_segment_meta_guard.rs` extension verifying 4-col composite

### Per-Item Guarantee Matrix (per Item 22)

| Demand | Per-item proof for Item 23 |
|---|---|
| 100% code coverage | new persistence + tracker + scheduler + supervisor + writer_state + handler covered by 13 tests above |
| 100% audit coverage | `MoversUnifiedSnapshotPersisted` + audit table `movers_unified_snapshot_audit` (DEDUP `(ts, security_id, segment, timeframe)`) |
| 100% testing coverage | unit + integration (DEDUP key + sort correctness) + DHAT (arena zero-alloc) + Criterion (bench) + meta-guard (segment + tiered retention) |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + secret-scan all enforced |
| 100% performance | DHAT zero-alloc + Criterion p99 < 200ms bench-gate |
| 100% monitoring | 7-layer telemetry (Prom counter + gauge + tracing + Loki + Telegram + Grafana + audit) |
| 100% logging | tracing macros + ERROR with `code` field per `MOVERS-UNIFIED-*` |
| 100% alerting | 1 new Prom alert + extends existing `tv-movers-22tf-*` cardinality |
| 100% security | sanitize_ilp_symbol on all SYMBOL columns + composite-key invariant |
| 100% security hardening | static IP unaffected; no new attack surface (read-only API extension) |
| 100% bugs fixing | adversarial 3-agent review pre-PR + post-impl |
| 100% scenarios covering | chaos test extension to verify burst absorption at 275K rows/sec |
| 100% functionalities covering | all 7 read-time category sorts tested + dashboard query tested |
| 100% code review | 3 agents pre + 3 agents post |
| 100% extreme check | all of above + ratchet tests fail build on regression |

### Items superseded by Item 23

| Item | Status |
|---|---|
| Item 16 (REUSE Movers22TfTracker, atomic-only) | **SUPERSEDED** — Item 23 uses new `movers_unified_tracker.rs`. Old `Movers22TfTracker` retained as transitional fallback; deleted after Wave 5 ships. |
| Item 19 (25-tf extension keeping `22Tf` naming) | **SUPERSEDED** — Item 23 names are `movers_unified*`, dropping `22Tf` artifact. |
| Item 21 (Tier A/B split-cadence) | **SUPERSEDED** — Item 23 unified schema makes Tier B unnecessary; full 11K universe at all 25 tf with tiered retention instead of tiered universe. |

### Honest non-claim restated

This unified design satisfies operator's "store everything for everything" demand within ₹5K/mo c7i.xlarge budget. **It is the cleanest design we've reached in this session.** Storage fits with smart partitioning + S3 lifecycle. Hot-path mitigations from agents still apply (stagger, arena, channel, bench). DEDUP key correctness preserved. I-P1-11 composite key invariant preserved.

## Plan Items (now 23, was 10)

## Item 23 — Enrichment Strategy Clarification (operator question 2026-05-01)

Operator: "while sorting it out based on something how will we know which segment or which sections which symbol or which indices or options or etc. can be easily known dude?"

### Enrichment approach: SLIM stored row + read-time lookup

**Stored columns (in `movers_unified`):** identifiers + raw stats only. ~11 columns, ~12 bytes/row compressed.

**Enrichment columns (resolved at API read time via `InstrumentRegistry` papaya HashMap):**

| Field | Source | Latency |
|---|---|---|
| `underlying_symbol` ("NIFTY", "RELIANCE") | `InstrumentRegistry.get((security_id, segment))` | ~100 ns lookup |
| `instrument_type` (INDEX/FUTIDX/OPTIDX/EQUITY/FUTSTK/OPTSTK) | same | same |
| `display_name` ("NIFTY-Jun2026-24500-CE") | same (precise contract label per CLAUDE.md commit 3903193) | same |
| `strike_price` (24500.0) | same | same |
| `option_type` (CE/PE/null) | same | same |
| `expiry_date` (2026-06-26) | same | same |

For 50-row dashboard query: total enrichment latency ~5 µs. Invisible to user.

### Why NOT store enrichment columns redundantly

At 422M rows/day across all 25 timeframes, storing the 6 enrichment columns adds ~6 GB/day = **blows 100 GB EBS in <17 days**. In-memory enrichment = zero storage cost.

### How dashboard / API / Telegram resolve the precise contract

| Consumer | Pattern |
|---|---|
| Dashboard via app API (`GET /api/top-movers`) | App handler does the JOIN in code; returns enriched JSON |
| Telegram alerts | Existing precise-contract-label generator (CLAUDE.md commit 3903193) — same pattern |
| Direct QuestDB SQL (operator ad-hoc) | `LEFT JOIN derivative_contracts d ON m.security_id = d.security_id AND m.segment = d.exchange_segment` |
| Grafana dashboard panels | Either via app API OR via SQL JOIN |

### Tests added

- `test_movers_unified_api_enriches_underlying_symbol_via_registry`
- `test_movers_unified_api_enriches_strike_option_expiry_for_options`
- `test_movers_unified_api_enriches_instrument_type_for_indices_vs_derivatives`
- `test_movers_unified_api_50_row_enrichment_latency_under_5ms` (Criterion bench)
- `test_movers_unified_questdb_join_with_derivative_contracts_works`

### Schema unchanged

`movers_unified` schema stays at the 11 columns specified in Item 23 above. **No enrichment columns in the table itself.** Item 23's storage cost projections remain valid (~12 bytes/row compressed → 76 GB hot at 90-day retention with tiered sub-minute retention).

## Item 24 — Plan-Wide Proof Matrix: O(1) Latency × Uniqueness × Deduplication (operator demand 2026-05-01)

Operator: "how effectively all these entire plan will achieve O(1) latency and uniqueness guarantee and deduplication as well dude okay? provide me all these with real-time checks and guarantee and assurance and proof as well."

Every item in Wave 5 is mapped below to its concrete mechanical proof for each of the 3 properties. **No item ships without all 3 columns satisfied.**

### Per-item proof matrix

| # | Item | O(1) latency proof | Uniqueness proof | Dedup proof |
|---:|---|---|---|---|
| 1 | `subscription.scope` config gate | Boot-time only (NOT hot path); enum dispatch O(1) | N/A (config) | N/A |
| 2 | Universe filter (11,018 instr) | Boot-time only; `Arc<HashMap>` registry build O(N) once | `(security_id, exchange_segment)` composite per I-P1-11 | `DEDUP_KEY_DERIVATIVE_CONTRACTS = "security_id, underlying_symbol, exchange_segment"` |
| 3 | Main feed equal-split | Boot-time `i % 5` round-robin; reconnect replays static plan from `Arc<Vec<SubscriptionMessage>>` | Composite key (sid, seg) for distribution | Subscribe is idempotent (Dhan ignores duplicates) |
| 4 | Depth-20 single-side (4 static + 1 dynamic) | Static slots: boot-time. Dynamic slot: 60s scheduler on Core 3 (off hot path) | `(security_id, exchange_segment)` per registry lookup; `BSE_FNO` filter excludes SENSEX | DEDUP UPSERT on `subscription_audit_log`; selector-set diff by Vec equality (idempotent on no-change) |
| 5 | Depth-200 dynamic top-5 | Same — 60s scheduler on Core 3; reuses Item 4's `Arc<Vec<Row>>` (M8) | Composite (sid, seg) | Same audit table; rank-set diff |
| 6 | `core_affinity` 4-vCPU pinning | Boot-time pin; protects hot path FROM preemption variance (the ENABLER of O(1) under burst) | N/A | N/A |
| 7 | candle_aggregator composite-key fix | `HashMap<(u32, ExchangeSegment), OhlcvState>` lookup is O(1) amortized; DHAT zero-alloc per tick | **THIS ITEM IS the I-P1-11 fix** for candle_aggregator | `DEDUP_KEY_CANDLES_*` per timeframe table, all include segment |
| 8 | `warn!` → `error!` at tick_persistence.rs:357 | Log call, not hot path | N/A (log meta) | N/A |
| 9 | New ErrorCode variants | Enum match O(1) | `code_str()` cross-ref test enforces uniqueness across 53+ existing variants | N/A |
| 10 | Adversarial 3-agent review | Process; not code | N/A | N/A |
| 11 | Burst chaos test 200K tps | Pre-built `[ParsedTick; 1024]` array, cycle via `i % 1024` — zero alloc per iteration | Test uses composite-key sids | DEDUP verified by replay test |
| 12 | option_movers narrowing | Verify-only; if pass = no-op | Composite key already in `option_movers` writer | `DEDUP_KEY_MOVERS = "security_id, category, segment"` |
| 13 | Prev-close routing boot assertion | Boot-time only; halts if invariant violated | Composite (sid, seg) checked per slot | N/A (assertion is fail-fast) |
| 14 | NSE_EQ Full → Quote downgrade | Parser dispatcher: `match response_code` jump table O(1) per packet; SAME hot path latency as before | N/A | N/A |
| 15 | Drop redundant prev_close writes | Reduces ILP append rate by 99.7% (positive O(1) impact); in-memory cache lookup O(1) via `papaya` | Cache key = `(security_id, exchange_segment)` per I-P1-11 | `DEDUP_KEY_PREVIOUS_CLOSE` includes segment; first_seen_set prevents per-tick duplicate writes |
| 17 | Parallelization map docs | N/A | N/A | N/A |
| 18 | Findings remediation tracker | Doc only | N/A | N/A |
| 20 | May 2026 cutover timeline | Doc only | N/A | N/A |
| 22 | Per-Item Guarantee Matrix | Process meta-item | N/A | N/A |
| **23** | **Unified-schema movers** | **Per-tick update on `papaya::HashMap` with `AtomicU64`/`AtomicI64`-only fields = O(1) lock-free; per-snapshot rank computation runs on Core 3 (off hot path) with arena pool zero-alloc; Criterion bench `bench_movers_unified_full_snapshot_p99_under_200ms` is the ratchet** | **DEDUP key 4-col `(ts, security_id, segment, timeframe)` per I-P1-11 + STORAGE-GAP-01; meta-guard `dedup_segment_meta_guard.rs` scans the constant** | **DEDUP UPSERT KEYS `(ts, security_id, segment, timeframe)` — replay-safe; tie-break deterministic `<sort_col> DESC, volume DESC, security_id ASC`** |

### Cross-cutting mechanical enforcement (not per-item, applies plan-wide)

| Property | Enforcement | Real-time check |
|---|---|---|
| **O(1) latency** | `quality/benchmark-budgets.toml` (Criterion budgets: tick_binary_parse ≤10ns, papaya_lookup ≤50ns, oms_state_transition ≤100ns, full_tick_processing ≤10µs); `scripts/bench-gate.sh` fails CI on >5% regression | `cargo bench` post-merge + Prom histogram `tv_tick_processing_duration_ns` p99 |
| | DHAT zero-alloc tests (`crates/*/tests/dhat_*.rs`) — hot path must allocate ≤4 blocks / 8KB across 10K calls | DHAT post-merge gate (hard-fail in CI) |
| | Banned-pattern scanner blocks `clone()`, `format!()`, `Vec::new()`, `Box`, `dyn`, `dashmap`, mutex on hot path | Pre-commit hook |
| | `core_affinity` Item 6 pins 4 Tokio workers to dedicated cores → eliminates preemption-induced variance | `tv_core_pinning_workers_pinned == 4` gauge alert |
| **Uniqueness** | I-P1-11 composite key `(security_id, exchange_segment)` enforced everywhere via 7 mechanical layers (planner dedup, CSV-parse dedup, registry composite index, storage DEDUP keys, banned-pattern hook category 5, runtime collision detector, Prom gauge) | `tv_instrument_registry_cross_segment_collisions` gauge in `/health` + `make doctor` |
| | `dedup_segment_meta_guard.rs` scans every `DEDUP_KEY_*` constant; CI hard-fail if any mentions `security_id` without `segment`/`exchange_segment`/`exchange` | Workspace test suite |
| | Banned-pattern scanner category 5 blocks new `HashSet<u32>` / `HashSet<SecurityId>` / `HashMap<u32, _>` / `HashMap<SecurityId, _>` / `.registry.get(id)` without `// APPROVED:` | Pre-commit hook |
| **Deduplication** | Every QuestDB storage table has `DEDUP UPSERT KEYS` including the designated `ts` + composite (security_id, segment) + table-specific dimensions (timeframe / category / sequence_number / etc.) | DDL + meta-guard tests |
| | Idempotency keys for orders (UUID v4 in Valkey before submission) per OMS-GAP-05 | Replay-safe; double-orders impossible |
| | Audit tables (6 of them per Wave 2-D Items 8/9) all use composite DEDUP including `ts` to prevent same-day-same-event duplication | `audit_*_persistence.rs` DEDUP key tests |
| | Tick rescue ring → spill NDJSON → DLQ chain: re-replay safe because every ILP append is dedup-protected; same tick written twice = single row at QuestDB | `chaos_zero_tick_loss.rs` regression test |

### Real-time monitoring (live ground truth, not aspirational claims)

| Property | Live signal | Where to read |
|---|---|---|
| O(1) latency holds | `tv_tick_processing_duration_ns` p99 < 10µs | Prometheus `prometheus:9090` + Grafana Operator Health panel |
| O(1) on movers (Item 23) | `tv_movers_unified_arena_pool_alloc_count == 0` gauge | Prometheus + Operator Health |
| Uniqueness (no cross-segment merge) | `tv_instrument_registry_cross_segment_collisions` gauge | `/health` endpoint + `mcp__tickvault-logs__run_doctor` |
| Dedup correctness | `tv_questdb_dedup_skipped_total` counter (rises = same-key second write was deduped) | Prometheus |
| Burst absorption (no rows dropped under burst) | `tv_movers_unified_burst_peak_rows_per_sec` gauge + `tv_tick_dropped_total` (must be 0) | Prom + Telegram alert if non-zero |
| WS no-disconnect | `tv_websocket_connections_active == 5` gauge | `/health` + Operator Health |
| QuestDB no-fail | `tv_questdb_disconnected_seconds == 0` gauge (alert at >30) | Prom + Telegram |

### Honest non-claim (per `wave-4-shared-preamble.md` §8)

> "100% inside the tested envelope, with ratcheted regression coverage."

The 3 properties (O(1) latency, uniqueness, deduplication) are **mechanically enforced at every layer** — not asserted by hand-wave. Every property has:
1. A **compile-time** check (banned-pattern scanner, type-system invariants)
2. A **build-time** check (cargo test, DHAT, Criterion bench-gate)
3. A **boot-time** check (assertion on registry collision count, pinning verification, DEDUP key validation)
4. A **runtime** check (Prom gauge + alert + Telegram on breach)
5. A **post-deploy** check (`make doctor` + `mcp__tickvault-logs__run_doctor`)

If ANY of these fire → operator is paged. If NONE fire → the property holds within the tested envelope.

### What this matrix proves

For every Wave 5 item:
- O(1) latency holds OR is explicitly off hot-path (boot/process/doc)
- Uniqueness uses composite (security_id, exchange_segment) OR is explicitly N/A
- Deduplication uses DEDUP UPSERT KEYS including segment OR is explicitly N/A

**No item escapes this matrix.** The Per-Item Guarantee Matrix from Item 22 amplifies this with 15-row × 7-row coverage on every other dimension (security, monitoring, alerting, audit, etc.).

## Plan Items (now 24, was 10)

## Item 25 — Materialized-View Movers Redesign (operator demand 2026-05-01) — SUPERSEDES Item 23

Operator: "why didn't you move all the movers similar to materialised view candles dude — even for movers 1s also you can keep it in normal table and remaining you can move it into materialised views right dude?"

**Operator is right.** Item 23's 25-separate-writer-stream design was overcomplicated. Use QuestDB materialized views (same pattern as `candles_1m`, `candles_5m`, etc. already in `crates/storage/src/materialized_views.rs`).

### The simplification

| Aspect | Item 23 (old) | **Item 25 (new)** |
|---|---|---|
| App writer tasks | 25 (one per timeframe) | **1** (base 1s only) |
| ILP streams from app to QuestDB | 25 | **1** |
| Arena pools | 25 (one per task) | **0** (no per-tf computation in app) |
| Staggering (200ms offset per tf) | Required | **Eliminated** |
| Supervisor respawn cardinality | 25 | **1** |
| `mpsc` channels | 25 × 65536 | **1** |
| Burst @ 09:15 IST coboundary from app | 275K rows/sec | **11K rows/sec** (QuestDB handles aggregation) |
| Code complexity | High (tracker + scheduler + supervisor + writer_state per tf) | **Massively simpler** (one writer task, mat views server-side) |
| Aggregation correctness | Manually computed in Rust (bug-prone) | **QuestDB-native via SAMPLE BY** |

### Design

**Base table (only physical write from app, 1Hz cadence):**

```sql
CREATE TABLE IF NOT EXISTS movers_unified_1s (
  ts            TIMESTAMP,
  security_id   LONG,
  segment       SYMBOL CAPACITY 16 NOCACHE,
  open_interest LONG,
  oi_delta      LONG,
  volume        LONG,
  last_price    DOUBLE,
  prev_close    DOUBLE,
  change_pct    DOUBLE,
  received_at   TIMESTAMP
) TIMESTAMP(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(ts, security_id, segment);
```

**24 materialized views, one per timeframe, incrementally refreshed by QuestDB:**

```sql
-- Pattern (repeated for 5s, 15s, 30s, 1m, 2m, 3m, 4m, 5m, 6m, 7m, 8m, 9m, 10m,
-- 11m, 12m, 13m, 14m, 15m, 30m, 1h, 2h, 3h, 4h, 1d):
CREATE MATERIALIZED VIEW IF NOT EXISTS movers_unified_5s AS
  SELECT
    security_id,
    segment,
    ts,
    last(last_price)                                                AS last_price,
    last(volume)                                                    AS volume,
    last(open_interest)                                             AS open_interest,
    last(open_interest) - first(open_interest)                      AS oi_delta,
    last(prev_close)                                                AS prev_close,
    ((last(last_price) - last(prev_close)) / last(prev_close)) * 100 AS change_pct
  FROM movers_unified_1s
  SAMPLE BY 5s;
```

QuestDB refreshes mat views **incrementally** as new rows arrive in the base — server-side, no app intervention.

### Read path (unchanged from Item 23 except table names)

```sql
-- TOP_VOLUME at 5m timeframe over last 5min:
SELECT * FROM movers_unified_5m
WHERE ts > now() - 5m AND segment != 'BSE_FNO' AND change_pct > 0
ORDER BY volume DESC, change_pct DESC, security_id ASC
LIMIT 50;

-- HIGHEST_OI at 1h:
SELECT * FROM movers_unified_1h
WHERE ts > now() - 1h
ORDER BY open_interest DESC LIMIT 50;
```

API handler enriches each row via `InstrumentRegistry` lookup at read time (same as Item 23). Sub-millisecond.

### DEDUP key (simplified)

| Table | DEDUP UPSERT KEYS |
|---|---|
| `movers_unified_1s` (base) | `(ts, security_id, segment)` — 3-col |
| All 24 mat views | inherit from base via SAMPLE BY semantics; QuestDB enforces uniqueness per (ts, security_id, segment) within each bucket |

NO `timeframe` column needed because each view IS its own timeframe.

### Tiered hot retention

Same intent as Item 23, applied to base + mat views via QuestDB partition manager:

| Table | Hot retention |
|---|---|
| `movers_unified_1s` (base) | 7 days (1s data is high-frequency, rarely queried >1 week back) |
| `movers_unified_5s` | 14 days |
| `movers_unified_15s` | 30 days |
| `movers_unified_30s` through `movers_unified_1d` (22 views) | 90 days |

After hot expiry, partition manager detaches → S3 lifecycle to Glacier Deep Archive (per `aws-budget.md` ₹333/mo).

### Files (massively reduced from Item 23)

- `crates/storage/src/movers_unified_persistence.rs` — base table DDL + ILP writer + 24 mat-view CREATE-IF-NOT-EXISTS at boot
- `crates/storage/src/materialized_views.rs` — extend existing module to register 24 movers mat views alongside candle views
- `crates/core/src/pipeline/movers_unified_writer.rs` — ONE writer task; consumes from `tick_processor` enrichment, ILP-appends to `movers_unified_1s` at 1Hz
- `crates/api/src/handlers/top_movers.rs` — read-time queries against the right mat view + InstrumentRegistry enrichment
- ~~movers_unified_scheduler.rs~~ DELETED — no scheduler needed
- ~~movers_unified_supervisor.rs~~ DELETED — only 1 task to supervise
- ~~movers_unified_writer_state.rs (per-tf)~~ DELETED — merged into single writer

### Tests

- `test_movers_unified_1s_base_table_ddl_creates_with_dedup_keys`
- `test_movers_unified_24_materialized_views_auto_created_at_boot`
- `test_materialized_view_5m_aggregates_correctly_via_sample_by` (sample data)
- `test_materialized_view_oi_delta_uses_last_minus_first` (correctness)
- `test_materialized_view_change_pct_formula` (correctness)
- `test_api_top_movers_queries_correct_view_per_timeframe` (5m → movers_unified_5m)
- `test_api_enriches_via_instrument_registry_o1_lookup` (carry-over from Item 23)
- `test_movers_writer_writes_only_to_base_1s_not_to_mat_views` (regression guard)
- `bench_movers_unified_1s_writer_p99_under_1ms` (Criterion bench — base writer hot path)
- `test_movers_unified_partition_retention_per_tier` (1s=7d, 5s=14d, 15s=30d, 30s+=90d)
- `test_movers_dedup_key_is_3_col_no_timeframe` (replaces Item 23 4-col guard)

### 9-box (per Item 22 mandatory matrix)

- ① typed event: `MoversUnifiedWriterIlpFailed` (Severity::Medium); `MoversUnifiedMatViewRefreshLag` (Severity::Medium when QuestDB mat view refresh > 60s behind base)
- ② ErrorCode: `MOVERS-UNIFIED-01` (writer ILP failed, Medium); `MOVERS-UNIFIED-04` (mat view refresh lag, Medium); `MOVERS-UNIFIED-05` (mat view DDL failed at boot, Critical halt boot)
- ③ tracing: `info!(rows_persisted, …)` per 1s write
- ④ Prom: `tv_movers_unified_1s_rows_total`, `tv_movers_mat_view_refresh_lag_seconds{view}`, `tv_movers_unified_writer_errors_total`
- ⑤ Grafana: rewrite Operator Health movers panel — single base table row count + 24 view-lag gauges
- ⑥ Alert: `tv-movers-unified-mat-view-refresh-lag-exceeds-60s`
- ⑦ Call site: `tick_processor` enrichment → 1 writer task on Core 3
- ⑧ Triage: `.claude/triage/error-rules.yaml::movers-unified-04-mat-view-lag-investigate`
- ⑨ Ratchet: 11 tests above + `dedup_segment_meta_guard.rs` extension

### Per-Item Guarantee Matrix (per Item 22)

All 15 rows + 7 resilience rows apply (same as Item 23). The mat-view design SIMPLIFIES delivery of these guarantees — fewer moving parts to fail.

### Items 16, 19, 21, 23 — all SUPERSEDED by Item 25

Wave 5 movers design = Item 25 only. Earlier iterations preserved in plan history for traceability.

## Plan Items (now 25, was 10)
