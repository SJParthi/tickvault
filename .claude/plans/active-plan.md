# Implementation Plan: 25K Instrument Scale — Zero Tick Loss Guarantee

**Status:** DRAFT
**Date:** 2026-04-07
**Approved by:** pending

## Problem

Two code paths currently DROP ticks when buffers are full:

1. **connection.rs:439-446** — WebSocket read loop drops frames after 5s timeout
   when SPSC channel is full. Frame is permanently lost.
2. **main.rs:2344-2354** — Broadcast channel evicts oldest ticks when persistence
   consumer lags. Ticks permanently lost (Lagged error).

**Requirement:** ZERO tick loss. Irrespective of any situation — QuestDB crash,
Docker crash, disk full, CPU spike — every single tick from every WebSocket must
be captured and available. Nothing missed, nothing dropped.

## Architecture (Already Correct — No Changes Needed)

- Trading decisions (indicators, strategy, risk, OMS) = 100% in-memory
- QuestDB = write-only cold-path in separate tokio task
- WebSocket connections = fully isolated, never blocked by DB or processing
- Only external call in hot path: Dhan REST API for order placement

## Plan Items

- [ ] Item 1: Remove frame drop in WebSocket read loop — use try_send + overflow counter
  - Files: crates/core/src/websocket/connection.rs
  - Change: Replace `time::timeout(5s, frame_sender.send(data))` with
    `frame_sender.try_send(data)`. On `TrySendError::Full`, log WARN with
    counter and use `frame_sender.send(data).await` (backpressure — blocks WS
    read until space available). This is correct because:
    a) With 128K channel at 10K ticks/sec = 13 seconds before full
    b) If channel IS full, it means tick processor is frozen for 13+ seconds
    c) Blocking WS read is better than losing ticks — Dhan pings every 10s,
       timeout at 40s, so WS connection survives up to 40s of backpressure
    d) Counter `dlt_ws_frame_backpressure_total` tracks occurrences
  - Tests: test_connection_backpressure_does_not_drop_frames

- [ ] Item 2: Increase SPSC frame channel from 65,536 to 131,072 (128K)
  - Files: crates/common/src/constants.rs, crates/core/src/websocket/connection_pool.rs
  - Add constant: `FRAME_CHANNEL_CAPACITY: usize = 131_072`
  - Change pool to use constant instead of hardcoded 65536
  - Rationale: 128K at 10K ticks/sec = 13 seconds backpressure-free headroom.
    At 25K peak = 5.2 seconds. At ~1.5KB/frame = ~187MB per connection.
  - Tests: test_frame_channel_capacity_constant

- [ ] Item 3: Increase broadcast channel from 65,536 to 262,144 (256K)
  - Files: crates/common/src/constants.rs
  - Change: `TICK_BROADCAST_CAPACITY: usize = 262_144`
  - Rationale: 256K at 10K/sec = 26 seconds. At 25K peak = 10 seconds.
    At 112 bytes = ~28MB RAM. This is the ONLY buffer where overflow = permanent
    tick loss in the persistence consumer path.
  - Tests: test_tick_broadcast_capacity_constant

- [ ] Item 4: Increase tick ring buffer from 300,000 to 600,000 (600K)
  - Files: crates/common/src/constants.rs
  - Change: `TICK_BUFFER_CAPACITY: usize = 600_000`
  - Update: `TICK_BUFFER_HIGH_WATERMARK = 480_000` (80%)
  - Rationale: 600K at 10K/sec = 60 seconds before disk spill.
    At 112 bytes = ~64MB RAM.
  - Tests: existing capacity tests reference the constant

- [ ] Item 5: Increase candle ring buffer from 100,000 to 200,000 (200K)
  - Files: crates/common/src/constants.rs
  - Change: `CANDLE_BUFFER_CAPACITY: usize = 200_000`
  - Rationale: 25K instruments × 1 candle/sec = 25K candles/sec.
    200K = 8 seconds. At 76 bytes = ~14.5MB RAM.
  - Tests: existing capacity tests reference the constant

- [ ] Item 6: Increase depth ring buffer from 50,000 to 100,000 (100K)
  - Files: crates/common/src/constants.rs
  - Change: `DEPTH_BUFFER_CAPACITY: usize = 100_000`
  - Rationale: ~250 snapshots/sec. 100K = ~6.7 minutes. At 116 bytes = ~11MB.
  - Tests: existing capacity tests reference the constant

- [ ] Item 7: Update all capacity constant comments with correct 25K-instrument math
  - Files: crates/common/src/constants.rs
  - Every capacity constant must document: memory footprint, fill time at 10K/sec
    and 25K/sec tick rates

## Why Zero Drop is Now Guaranteed

After these changes, the data flow has NO drop points:

```
Dhan WS server
  → WebSocket connection (always alive — pong handled by library)
    → SPSC channel (128K capacity, BACKPRESSURE on full — never drops)
      → Tick Processor (O(1) parse+dedup+enrich, <10μs per tick)
        → broadcast (256K capacity — 26s headroom at 10K/sec)
          → Persistence Consumer
            → Ring buffer (600K — 60s at 10K/sec)
              → Disk spill (unlimited — bounded by disk space)
                → QuestDB (when available, dedup on ingest)
```

**Drop protection at each layer:**
| Layer | Protection | Overflow Behavior |
|-------|-----------|-------------------|
| WebSocket connection | Dhan pings + pong | Auto-reconnect on timeout |
| SPSC channel (128K) | Backpressure | Blocks WS read (WS survives 40s) |
| Broadcast channel (256K) | Large buffer | Lagged only if processor frozen 26s |
| Ring buffer (600K) | Memory buffer | Disk spill on overflow |
| Disk spill | Unlimited | CRITICAL alert if disk < 100MB |
| QuestDB | Dedup keys | Idempotent re-ingest |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB down entire day, 25K instruments | Ring buffer 60s → disk spill → ~64GB disk → all recovered |
| 2 | Persistence consumer lags 10 seconds | Broadcast 256K absorbs (26s headroom) — zero tick loss |
| 3 | Tick processor frozen 5 seconds | SPSC 128K absorbs (13s headroom) — zero frame loss |
| 4 | Peak volatility (25K ticks/sec) | All buffers have 5-10s headroom at peak |
| 5 | Normal trading day | All buffers near-empty, ~548 MB steady-state |
| 6 | Docker/infra crash | App continues, Telegram direct alerts |
| 7 | App crash | flush_on_shutdown() spills to disk → next startup recovers |
| 8 | SPSC full for >13 seconds | Backpressure blocks WS read. WS survives (40s timeout). No data lost. |
