# D2a BLOCKER — `start_dhan_lane` pure-extraction precondition is NOT met

**Status:** BLOCKED (reported to coordinator; no code shipped — Rule 14 anti-skeleton)
**Date:** 2026-06-26
**Branch:** `claude/d2a-start-dhan-lane`
**Authority:** `audit-findings-2026-04-17.md` Rule 14 (no skeleton/behaviour-changing refactor) +
`per-wave-guarantee-matrix.md` (behaviour-identical requirement) + the D2 design
(`active-plan-dhan-cold-start-d2.md` §1.0/§1.7 — D2a is a "pure extraction of the **now-isolated**
Dhan spine").

## One-line finding

D2a's stated precondition — *"the now-isolated slow-arm Dhan spine"* — does **not** hold on the
live Dhan-ON path. D2-pre (#1207) added a parallel `run_shared_infra_only` fn for the **Dhan-OFF**
path but left the **Dhan-ON slow arm byte-for-byte unchanged**, so the PROCESS-shared infra
(`tick_broadcast_sender`, 21-TF aggregator, API server, run-loop) is still **interleaved line-by-line**
with the Dhan-LANE work. A contiguous `start_dhan_lane(...)` that contains the Dhan stages while
leaving the shared infra in `main()` **cannot preserve spawn order** → not behaviour-identical.

## The irreducible interleaving (verified `main.rs`, latest main @ 3343abdc)

Slow-arm spawn order (2059→5177), with owner per design §1.1:

| # | line | spawn / binding | owner |
|---|------|-----------------|-------|
| 1 | 2158 | `TokenManager::initialize` (auth) | LANE |
| 2 | 2225 | dual-instance SSM lock + heartbeat | LANE-ish |
| 3 | 2368 | Dhan static-IP boot gate | LANE |
| 4 | 2521 | QuestDB readiness / 2544 clock-skew | PROCESS |
| 5 | 2694 | **seal-writer loop** + `set_global_seal_sender` (2679) | **PROCESS** |
| 6 | 2749/2781 | first-seen-set reset, disk-health watcher | PROCESS |
| 7 | 2931 | `load_instruments` → subscription plan / universe | LANE input |
| 8 | 3004 | `set_global_token_manager` (C4 site) | LANE |
| 9 | 3044 | `create_websocket_pool` → `pool_receiver` | **LANE** |
| 10 | 3130 | **`tick_broadcast_sender` created** (PROCESS resource) | **PROCESS** |
| 11 | 3139 | heartbeat watchdog | mixed |
| 12 | 3151 | slow-boot observability consumer (subscribes broadcast) | PROCESS |
| 13 | **3249** | **`spawn_engine_b_aggregator(&tick_broadcast_sender, …)`** (seals BOTH feeds) | **PROCESS** |
| 14 | 3261 | tick-storage broadcast consumer | PROCESS |
| 15 | 3284→3582 | **tick processor** (`run_tick_processor`, publishes into broadcast) | **LANE** |
| 16 | 3638 | `shutdown_notify` created | mixed |
| 17 | 3659 | `spawn_websocket_connections` (pool spawn) | **LANE** |
| 18 | 3660 | `spawn_pool_watchdog_task` (the `process::exit(2)` watchdog) | **LANE** |
| 19 | 3722–4591 | day-OHLC, post-market, depth-dynamic, option-chain schedulers | mixed |
| 20 | 4665 | order-update WS (`run_order_update_connection`) | **LANE** |
| 21 | 4759 | trading pipeline (`spawn_trading_pipeline_full`, subscribes broadcast) | PROCESS-ish |
| 22 | **4850** | **API server (`axum::serve`)** | **PROCESS** |
| 23 | 4859 | token-renewal (`spawn_renewal_task`) | **LANE** |
| 24 | 4872 | mid-session profile watchdog | **LANE** |
| 25 | 4890 | token sweep | **LANE** |
| 26 | 5024 | periodic health check | PROCESS |
| 27 | 5162 | `run_shutdown_fast` (the main run-loop) | PROCESS |

The two structurally-fatal interleavings:

1. **PROCESS aggregator (#13 @3249) sits between LANE WS-pool-create (#9 @3044) and LANE
   tick-processor (#15 @3582).** The aggregator + tick-storage + observability MUST `.subscribe()`
   to `tick_broadcast_sender` **before** the tick processor publishes, or the broadcast `Lagged`.
   The order `aggregator-subscribe (PROCESS) → tick-processor (LANE) → ws-pool-spawn (LANE)` is
   **load-bearing**, not incidental.
2. **PROCESS API server (#22 @4850) sits between LANE order-update (#20) and LANE token-renewal
   (#23).** `tick_broadcast_sender` itself (#10 @3130) is created **inside** the slow arm but is a
   PROCESS resource (aggregator/trading/tick-storage/observability all subscribe to it).

## Why neither workaround is allowed

- **Reorder so all LANE work is contiguous, shared infra around it** → changes observable spawn
  order (e.g. aggregator would subscribe after the WS pool spawns, or the API server would bind
  before order-update) → **NOT behaviour-identical** (the design's own gate + the 15-row matrix).
- **Split the extraction so `start_dhan_lane` is called twice / spine fragmented across
  `main()`↔lane↔`main()`↔lane** → a half-wired, non-contiguous "extraction" = the skeleton
  anti-pattern (Rule 14) and not the single callable cold-start fn D2b needs.

## What D2-pre was supposed to do but did NOT (the real gap)

Design §1.0 / §6 sub-PR table: D2-pre should have **hoisted** `tick_broadcast_sender`, the
seal-writer + aggregator, the API server, and the run-loop **out of the Dhan block** so they
construct **before** the Dhan ON/OFF branch and are **shared** by both paths, AND split
`run_shutdown_fast` into a PROCESS run-loop + a real graceful teardown.

What #1207 actually shipped: a **duplicate** `run_shared_infra_only` for the OFF path only
(it creates its OWN `tick_broadcast_sender` + aggregator + API — see `main.rs:6769`, comment
*"Groww runs its OWN aggregator instance"*). `run_shutdown_fast` was **not** split (still one fn
@6852; no `teardown_dhan_lane_tasks`). So the ON-path isolation D2a depends on never happened.

## The `DhanLaneContext` field list that the extraction WOULD need (mapped, not shipped)

From the locals the slow-arm Dhan stages read (would become ctx fields once the shared infra is
genuinely hoisted): `config`, `notifier`, `trading_mode`, `token_manager` (LANE-owned per C4 —
goes to `DhanLaneHandles`, not ctx), `trading_calendar`, `prev_day_cache`, `tick_storage`,
`feed_health`, `feed_runtime`, `health_status`, `tick_broadcast_sender` (PROCESS — must be hoisted
first), `tick_enricher` / `prev_oi_cache`, `subscription_plan`/`slow_boot_universe` (lane-built),
`questdb` config, `otel_provider`, `ws_frame_spill`, `shutdown_notify` (lane Notify), audit ILP
senders (ws_event), `instance_lock_shutdown_chain`, the `dhan_flag` `Arc<AtomicBool>`.
This is the **target** list per design L11; it is NOT final because it depends on the hoist that
has not landed.

## Recommendation

Do D2-pre **properly first** (hoist `tick_broadcast_sender` + aggregator + seal-writer + API +
run-loop out of the Dhan-ON block so they are shared, and split `run_shutdown_fast`), then D2a
becomes the pure, contiguous, behaviour-identical extraction the design assumes. Attempting D2a on
the current `main.rs` forces a behaviour change (reorder) or a skeleton (fragmented extraction) —
both barred.
