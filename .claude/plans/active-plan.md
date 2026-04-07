# Implementation Plan: 25K Instrument Scale — Buffer Capacity Fix

**Status:** DRAFT
**Date:** 2026-04-07
**Approved by:** pending

## Problem

Current buffer capacities were sized for ~1,000-5,000 ticks/sec. With 25,000 instruments
(5 connections × 5,000 each), peak tick rates are 25,000-50,000 ticks/sec. This creates
two critical risks:

1. **Broadcast channel (65,536)** — at 50K ticks/sec, overflows in 1.3 seconds. Dropped
   ticks NEVER reach the ring buffer or disk spill — they are **permanently lost**.
2. **Ring buffer (300,000)** — fills in 6-12 seconds at peak. Comment says "~5 minutes"
   which is wrong at 25K instrument scale.

## Plan Items

- [ ] Item 1: Increase broadcast channel capacity from 65,536 to 524,288 (512K)
  - Files: crates/common/src/constants.rs
  - Rationale: 524,288 at 50K ticks/sec = ~10 seconds headroom. At ~64 bytes/tick = ~32MB RAM.
    This is the ONLY buffer where overflow = permanent tick loss. Must be large.
  - Update comment with correct math for 25K instruments
  - Tests: test_tick_broadcast_capacity_sufficient_for_25k_instruments

- [ ] Item 2: Increase tick ring buffer from 300,000 to 1,500,000 (1.5M)
  - Files: crates/common/src/constants.rs
  - Rationale: 1.5M at 50K ticks/sec = 30 seconds before disk spill kicks in.
    At ~64 bytes = ~91MB RAM. Gives meaningful buffering before disk I/O.
  - Update high watermark (80% = 1,200,000)
  - Update comment with correct math
  - Tests: existing capacity tests still valid (they reference the constant)

- [ ] Item 3: Increase candle ring buffer from 100,000 to 500,000 (500K)
  - Files: crates/common/src/constants.rs
  - Rationale: 25K instruments × 1 candle/sec = 25K candles/sec.
    500K = 20 seconds at peak. At ~48 bytes = ~23MB RAM.
  - Update comment with correct math
  - Tests: existing capacity tests still valid

- [ ] Item 4: Increase depth ring buffer from 50,000 to 250,000 (250K)
  - Files: crates/common/src/constants.rs
  - Rationale: 200 instruments at ~200 snapshots/sec = ~200/sec.
    250K = ~20 minutes. Generous — depth data is lower volume.
  - Update comment with correct math
  - Tests: existing capacity tests still valid

- [ ] Item 5: Update SPSC frame channel from 65,536 to 131,072 (128K)
  - Files: crates/core/src/websocket/connection_pool.rs
  - Rationale: SPSC between WebSocket reader and tick processor.
    128K at 50K ticks/sec = ~2.6 seconds. If this lags, WebSocket read loop
    blocks (backpressure) which is better than dropping.
  - Tests: existing connection pool tests

- [ ] Item 6: Update all constant comments with correct 25K-instrument math
  - Files: crates/common/src/constants.rs
  - Every capacity constant must document:
    1. Total memory footprint
    2. Time-to-fill at 25K instruments peak (50K ticks/sec)
    3. Time-to-fill at 25K instruments average (15K ticks/sec)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB down entire day (6h 15m), 25K instruments | Ring buffer fills in 30s → disk spill activated → ~21GB disk used → all recovered on DB return |
| 2 | Tick persistence consumer lags 5 seconds | Broadcast 512K buffer absorbs burst without dropping ticks |
| 3 | Peak volatility (50K ticks/sec for 60s) | Broadcast absorbs ~10s, ring buffer absorbs ~30s, then disk spill |
| 4 | GC pause or I/O spike causes 2-second consumer stall | 512K broadcast = 10s headroom — no tick loss |
| 5 | Normal trading day, no outage | All buffers stay near-empty, minimal RAM overhead (~150MB total) |

## Memory Budget

| Buffer | Old Size | New Size | RAM (old) | RAM (new) |
|--------|---------|---------|-----------|-----------|
| Broadcast channel | 65K × 64B | 512K × 64B | ~4MB | ~32MB |
| Tick ring buffer | 300K × 64B | 1.5M × 64B | ~19MB | ~91MB |
| Candle ring buffer | 100K × 48B | 500K × 48B | ~5MB | ~23MB |
| Depth ring buffer | 50K × 116B | 250K × 116B | ~5.5MB | ~28MB |
| SPSC channel | 65K × ~128B | 128K × ~128B | ~8MB | ~16MB |
| **Total** | | | **~42MB** | **~190MB** |

190MB total RAM for buffers on a machine with 16GB+ is fine (1.2% of RAM).
