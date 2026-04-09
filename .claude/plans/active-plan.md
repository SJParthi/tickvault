# Implementation Plan: OBI Indicator + QuestDB Scaling + Remaining Gaps

**Status:** DRAFT
**Date:** 2026-04-09
**Approved by:** pending
**Branch:** `claude/fix-websocket-market-depth-C7poN`
**Previous session:** 9 commits covering WebSocket depth fixes, timestamps, indicators, infra

## Context

Previous session completed 9 commits fixing:
- Depth 24/7 connections, per-underlying metrics, first-frame Telegram
- market_depth exchange_timestamp + previous_close received_at
- H1 main feed disconnect/reconnect Telegram alerts
- Depth reconnection counters, 300s timeout (ping/pong per Dhan docs)
- Indicator precision (round to 2 decimals matching Dhan)
- L1 depth rebalancer Telegram alerts
- Previous close force-flush to QuestDB (was losing 28 packets)
- Docker --force-recreate for config updates (Alloy/Grafana)
- QuestDB 8GB memory + 4 CPUs + disconnect-on-error disabled

## Plan Items

### Phase A: OBI (Order Book Imbalance) Indicator

- [ ] 1. Create `obi_snapshots` QuestDB table with DDL
  - Files: `crates/storage/src/obi_persistence.rs` (new), `crates/storage/src/lib.rs`
  - Columns: segment, security_id, underlying, obi (f64), weighted_obi, total_bid_qty, total_ask_qty, bid_levels, ask_levels, max_bid_wall_price, max_bid_wall_qty, max_ask_wall_price, max_ask_wall_qty, spread, ts, received_at
  - Tests: DDL creation, append, flush, roundtrip

- [ ] 2. Implement OBI computation engine
  - Files: `crates/trading/src/indicator/obi.rs` (new), `crates/trading/src/indicator/mod.rs`
  - OBI = (total_bid_qty - total_ask_qty) / (total_bid_qty + total_ask_qty)
  - Weighted OBI = weight by distance from LTP (closer levels matter more)
  - Wall detection = level where qty > 10x average
  - Spread = ask_price_1 - bid_price_1
  - All O(1) — fixed 20-level iteration, no allocation
  - Tests: unit tests for all formulas, edge cases (zero qty, single level)

- [ ] 3. Wire OBI into depth processing pipeline
  - Files: `crates/app/src/main.rs` (depth frame consumer)
  - On every 20-level depth snapshot (both bid+ask received), compute OBI
  - Persist to `obi_snapshots` table
  - Add Prometheus metric: `dlt_obi_value` gauge per underlying

- [ ] 4. Grafana OBI dashboard
  - Files: `deploy/docker/grafana/dashboards/trading-pipeline.json`
  - Real-time OBI chart per underlying
  - Wall detection panel
  - Spread panel

- [ ] 5. Backtest SQL queries
  - Files: `docs/analysis/obi-backtest-queries.md` (new)
  - Correlation of OBI with price movement
  - Wall absorption rate analysis

### Phase B: QuestDB Scaling (100-500GB Future-Proof)

- [ ] 6. Implement partition management
  - Files: `crates/storage/src/partition_manager.rs` (new)
  - Auto-detach partitions older than 90 days (configurable)
  - Run daily at 16:30 IST (after market close)
  - Tests: partition age calculation, detach SQL

- [ ] 7. S3 cold storage archival
  - Files: `crates/storage/src/s3_archival.rs` (new)
  - Export to S3 as Parquet before detaching
  - SEBI 5-year retention compliance

- [ ] 8. QuestDB WAL optimization
  - Files: `deploy/docker/docker-compose.yml`
  - Tune WAL segment size, uncommitted rows, connection limits

### Phase C: Remaining WebSocket Gaps

- [ ] 9. C1: Graceful unsubscribe on shutdown (send RequestCode 12)
  - Files: `crates/core/src/websocket/connection.rs`

- [ ] 10. H2: Pool-level circuit breaker (all 5 fail = HALT)
  - Files: `crates/core/src/websocket/connection_pool.rs`

- [ ] 11. H3: Per-security stale tick detection
  - Files: `crates/core/src/pipeline/tick_processor.rs`

### Phase D: AWS Deployment Hardening

- [ ] 12. Resource validation on boot (memory, disk, map_count)
  - Files: `crates/app/src/infra.rs`
  - HALT if under-resourced — never run degraded

- [ ] 13. EBS auto-expand for QuestDB data volume
  - Files: deployment scripts

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | OBI on 20-level depth | Value -1.0 to +1.0 in QuestDB per snapshot |
| 2 | 90 days data (~270GB) | Auto-partition detach keeps hot data <50GB |
| 3 | All 5 WS connections fail | Circuit breaker CRITICAL halt |
| 4 | AWS deploy from Mac config | Same compose, zero changes needed |
| 5 | QuestDB restart | ILP auto-reconnects, zero data loss (ring buffer) |
| 6 | 500GB disk usage | S3 archival + partition detach manages automatically |

## Key Files to Read First

1. `CLAUDE.md` — project rules, banned patterns, codebase structure
2. `.claude/rules/project/websocket-enforcement.md` — resolved + open WS gaps
3. `.claude/rules/dhan/full-market-depth.md` — depth protocol (20+200 level)
4. `crates/core/src/websocket/depth_connection.rs` — depth WS code
5. `crates/storage/src/tick_persistence.rs` — QuestDB persistence patterns
6. `crates/storage/src/deep_depth_persistence.rs` — deep depth storage
7. `crates/trading/src/indicator/engine.rs` — indicator computation pattern
8. `deploy/docker/docker-compose.yml` — QuestDB config

## Dhan Issues Pending (External)

1. **200-level depth**: ResetWithoutClosingHandshake — email sent to apihelp@dhan.co (Client ID: 1106656882), forum thread #63620. Waiting for response.
2. **Previous close only 28/25000**: Dhan only sends prev_close for Ticker mode (IDX_I), not Full mode subscriptions mid-session. Not a code bug — Dhan server behavior. Try starting before 9:15 AM to get full burst.
