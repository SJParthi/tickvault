# Implementation Plan: 25K Instrument Scale — Buffer Capacity Fix

**Status:** DRAFT
**Date:** 2026-04-07
**Approved by:** pending

## Problem

Current buffer capacities were sized for ~1,000-5,000 ticks/sec. With 25,000 instruments
(5 connections × 5,000 each), realistic tick rates are 3,900-10,000 ticks/sec with peaks
up to 25,000/sec during market open/close. This creates two critical risks:

1. **Broadcast channel (65,536)** — at 10K ticks/sec, overflows in 6.5 seconds. Dropped
   ticks NEVER reach the ring buffer or disk spill — they are **permanently lost**.
2. **SPSC frame channel (65,536)** — same risk. If full, WebSocket drops frames after 5s
   timeout to keep the WS connection alive.
3. **Ring buffer comments are wrong** — say "~5 minutes" but at 10K/sec it's only 30 seconds.

## Architecture Confirmation (No Changes Needed)

The hot-path architecture is **already correct**:
- Trading decisions (indicators, strategy, risk, OMS) are 100% in-memory
- QuestDB is write-only cold-path in a separate tokio task
- WebSocket connections are fully isolated from everything else
- Only external call in trading path: Dhan REST API for order placement

## Complete Memory Budget (All Components)

### Fixed/Constant (Always Allocated)
| Component | Memory | Notes |
|-----------|--------|-------|
| SPSC frame channels (5 × 65K × 1.5KB) | 469 MB | WebSocket → processor |
| ILP buffers (6 writers × 8MB) | 46 MB | QuestDB batch senders |
| Indicator engine (25K × 512B) | 12.8 MB | SMA/EMA/RSI/MACD/BB per instrument |
| Broadcast channel (65K × 112B) | 7 MB | Tick fan-out |
| Instrument registry (25K × 200B) | 4.8 MB | papaya concurrent map |
| Candle aggregator (25K × 144B) | 3.4 MB | Per-instrument live candle |
| Strategy evaluator (25K × 64B) | 1.5 MB | Per-instrument FSM state |
| Greeks + top movers + misc | 2.5 MB | Option meta, spot prices |
| Dedup ring (65K × 8B) | 0.5 MB | Tick deduplication |
| **Subtotal** | **~548 MB** | |

### QuestDB-Down (Ring Buffers Fill)
| Component | Memory | Fill Time @10K/sec |
|-----------|--------|-------------------|
| Deep depth ring (50K × 3,216B) | 153 MB | ~4 min (low rate) |
| Tick ring (300K × 112B) | 32 MB | 30s |
| Candle ring (100K × 76B) | 7 MB | 4s (25K candles/sec) |
| Depth ring (50K × 116B) | 6 MB | ~4 min (low rate) |
| **Subtotal** | **+198 MB** | Then → disk spill |

### Cold-Path (Periodic, Temporary)
| Component | Memory | Frequency |
|-----------|--------|-----------|
| Option chain fetch | 17 MB | Every 60s |
| Historical candle cache | 10 MB | Startup only |
| **Subtotal** | **+27 MB** | |

### GRAND TOTAL: ~773 MB worst case (5% of 16GB RAM)

## Plan Items

- [ ] Item 1: Increase broadcast channel capacity from 65,536 to 262,144 (256K)
  - Files: crates/common/src/constants.rs
  - Rationale: 262,144 at 10K ticks/sec = ~26 seconds headroom (was 6.5s).
    At 25K peak = ~10 seconds. At 112 bytes/tick = ~28MB RAM.
    This is the ONLY buffer where overflow = permanent tick loss. Must be large.
  - Update TICK_BROADCAST_CAPACITY constant + comment
  - Tests: test_tick_broadcast_capacity_constant

- [ ] Item 2: Increase tick ring buffer from 300,000 to 600,000 (600K)
  - Files: crates/common/src/constants.rs
  - Rationale: 600K at 10K ticks/sec = 60 seconds before disk spill.
    At 25K peak = 24 seconds. At 112 bytes = ~64MB RAM.
  - Update TICK_BUFFER_CAPACITY + TICK_BUFFER_HIGH_WATERMARK (80% = 480,000)
  - Update comment with correct math
  - Tests: existing capacity tests reference the constant

- [ ] Item 3: Increase candle ring buffer from 100,000 to 200,000 (200K)
  - Files: crates/common/src/constants.rs
  - Rationale: 25K instruments × 1 candle/sec = 25K candles/sec.
    200K = 8 seconds at peak. At 76 bytes = ~14.5MB RAM.
  - Update CANDLE_BUFFER_CAPACITY + comment
  - Tests: existing capacity tests reference the constant

- [ ] Item 4: Increase depth ring buffer from 50,000 to 100,000 (100K)
  - Files: crates/common/src/constants.rs
  - Rationale: ~250 depth snapshots/sec for 5-level depth.
    100K = ~6.7 minutes. At 116 bytes = ~11MB RAM.
  - Update DEPTH_BUFFER_CAPACITY + comment
  - Tests: existing capacity tests reference the constant

- [ ] Item 5: Increase SPSC frame channel from 65,536 to 131,072 (128K)
  - Files: crates/core/src/websocket/connection_pool.rs
  - Rationale: SPSC between WebSocket reader and tick processor.
    128K at 10K ticks/sec = ~13 seconds before WS drops frames.
    At 25K peak = ~5 seconds. WebSocket stays alive (drops frame, not connection).
  - Update channel capacity in `ConnectionPool::new()`
  - Add constant FRAME_CHANNEL_CAPACITY = 131_072 in constants.rs
  - Tests: existing connection pool tests

- [ ] Item 6: Update all constant comments with correct 25K-instrument math
  - Files: crates/common/src/constants.rs
  - Every capacity constant must document:
    1. Total memory footprint
    2. Time-to-fill at 10K ticks/sec (realistic peak)
    3. Time-to-fill at 25K ticks/sec (extreme peak)

## Updated Memory Budget (After Changes)

| Buffer | Old | New | Old RAM | New RAM | Fill @10K/s | Fill @25K/s |
|--------|-----|-----|---------|---------|-------------|-------------|
| Broadcast | 65K | 256K | 7 MB | 28 MB | 26s | 10s |
| Tick ring | 300K | 600K | 32 MB | 64 MB | 60s | 24s |
| Candle ring | 100K | 200K | 7 MB | 14.5 MB | 8s | 8s |
| Depth ring | 50K | 100K | 6 MB | 11 MB | 6.7m | 6.7m |
| SPSC channel | 65K | 128K | 469 MB | 938 MB | 13s | 5s |
| **Total change** | | | **+521 MB** | | | |

NOTE: SPSC channel dominates because each frame is ~1.5KB (raw WebSocket binary).
Total app memory goes from ~773 MB to ~1.29 GB. Still <10% of 16GB RAM.

## Disk for Full-Day QuestDB Outage

| Data | Rate | Records/Day | Disk |
|------|------|-------------|------|
| Ticks | 10K/sec | 225M | 23.5 GB |
| 1s Candles | 25K/sec | 562M | 39.8 GB |
| Depth (5-level) | 250/sec | 5.6M | 0.6 GB |
| Deep depth | 5/sec | 112K | 0.3 GB |
| **Total** | | | **~64 GB** |

Requires 128GB+ disk. MacBook 256GB default = fine. AWS EBS = size accordingly.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB down entire day, 25K instruments | Ring buffer 60s → disk spill → ~64GB disk → all recovered on DB return |
| 2 | Tick persistence consumer lags 10 seconds | Broadcast 256K buffer absorbs burst (26s headroom) — no tick loss |
| 3 | Peak volatility (25K ticks/sec for 60s) | Broadcast absorbs 10s, ring buffer 24s, then disk spill |
| 4 | CPU spike causes 5-second consumer stall | 256K broadcast + 128K SPSC = safe margin |
| 5 | Normal trading day, no outage | All buffers near-empty, ~548 MB steady-state |
| 6 | Docker/infra crash (Prometheus/Grafana) | App continues, Telegram direct alerts still work |
| 7 | App crash during market hours | flush_on_shutdown() spills to disk → next startup recovers |
