# Implementation Plan: Comprehensive Resilience, Testing & Automation Hardening

**Status:** DRAFT
**Date:** 2026-04-04
**Approved by:** pending

## Context

Based on deep research across 12 parallel agents covering: QuestDB resilience, monitoring/automation,
parser verification, Greeks/depth, Dhan docs (live feed, annexure, depth, order update), Python SDK,
and testing infrastructure.

## Research Findings Summary

### What's Already Solid (Verified)
1. **Parser byte offsets** — ALL match Dhan specs exactly (100% verified across all 9 parser files)
2. **Tick persistence QuestDB resilience** — 3-layer defense: 300K ring buffer → disk spill (112B fixed records) → startup recovery
3. **Monitoring** — 42 Prometheus alert rules, 27 Telegram event types, heartbeat watchdog every 30s
4. **Tick pipeline** — Non-blocking on QuestDB failure, ticks broadcast to trading logic regardless
5. **Token refresh** — arc-swap atomic swap, exponential backoff, auto-renewal 1h before expiry
6. **Circuit breaker** — 3-state FSM (Closed/Open/HalfOpen) with auto-recovery
7. **Rate limiter** — GCRA 10/sec SEBI-compliant via governor crate
8. **Dry-run mode** — Default ON (`dry_run = true`), sandbox enforcement
9. **7,350 tests** across 22 test type categories
10. **Dhan enum mappings** — All ExchangeSegment, FeedRequest/Response codes, error codes verified correct
11. **All LE reads** — Zero `from_be_bytes` calls in parser (verified)
12. **f32 vs f64 correct** — Live Feed uses f32, Full Market Depth uses f64 (verified in code)

### CRITICAL GAPS Found (from deep agent research)

**GAP-1: Depth persistence MISSING startup recovery**
- Tick has `recover_stale_spill_files()` called at main.rs:905
- Depth has NO equivalent method — spill files from crashes are LOST
- **Risk: Depth data loss on app restart during QuestDB outage**

**GAP-2: Depth persistence MISSING graceful shutdown**
- Tick has full `flush_on_shutdown()` (4-step: flush → drain ring → spill to disk → flush BufWriter)
- Depth only calls `force_flush()` at tick_processor.rs:1010-1014
- **Risk: Depth snapshots stranded in ring buffer on shutdown**

**GAP-3: Candle persistence recovery EXISTS but NEVER CALLED at startup**
- `CandlePersistenceWriter::recover_stale_spill_files()` exists at candle_persistence.rs:756
- But main.rs never calls it — orphaned candle spill files ignored
- **Risk: Candle data loss after crash+restart**

**GAP-4: Candle persistence MISSING graceful shutdown**
- Only calls `force_flush()` at tick_processor.rs:1043-1045
- Missing ring buffer drain + disk spill on shutdown
- **Risk: Candles stranded in ring buffer on shutdown**

**GAP-5: Depth persistence MISSING drop counter**
- Tick tracks `ticks_dropped_total` metric
- Depth has no equivalent — data loss is invisible
- **Risk: Silent data loss with no alerting**

**GAP-6: Clippy warnings** (ALREADY FIXED)
- 2 collapsible_if in main.rs — fixed in this session

**GAP-7: Sandbox enforcement not mechanically enforced until June**
- `dry_run = true` is default but no CI gate blocking live orders before July

## Plan Items

### Priority 1: Data Loss Prevention (CRITICAL)

- [ ] **Item 1: Add `flush_on_shutdown()` to DepthPersistenceWriter**
  - Files: `crates/storage/src/tick_persistence.rs` (DepthPersistenceWriter section)
  - Tests: `test_depth_flush_on_shutdown_spills_to_disk`, `test_depth_shutdown_drains_ring_buffer`
  - Action: Implement 4-step shutdown matching tick persistence (flush → drain ring → spill to disk → flush BufWriter)

- [ ] **Item 2: Add `recover_stale_spill_files()` to DepthPersistenceWriter**
  - Files: `crates/storage/src/tick_persistence.rs` (DepthPersistenceWriter section)
  - Tests: `test_depth_recover_stale_spill_files_on_startup`
  - Action: Implement recovery method matching tick persistence pattern

- [ ] **Item 3: Call depth recovery at startup in main.rs**
  - Files: `crates/app/src/main.rs` (around line 951-973)
  - Tests: Verify startup flow calls `depth_writer.recover_stale_spill_files()`
  - Action: Add recovery call after DepthPersistenceWriter creation

- [ ] **Item 4: Call candle recovery at startup in main.rs**
  - Files: `crates/app/src/main.rs`, `crates/core/src/pipeline/tick_processor.rs`
  - Tests: Verify candle stale spill recovery is called
  - Action: Add recovery call where candle writer is created

- [ ] **Item 5: Add `flush_on_shutdown()` to candle writer**
  - Files: `crates/storage/src/candle_persistence.rs`
  - Tests: `test_candle_flush_on_shutdown_spills_to_disk`
  - Action: Implement shutdown matching tick persistence, update tick_processor.rs shutdown

- [ ] **Item 6: Call candle + depth shutdown in tick_processor.rs**
  - Files: `crates/core/src/pipeline/tick_processor.rs` (shutdown section around line 1008-1045)
  - Tests: `test_shutdown_flushes_all_three_writers`
  - Action: Replace `force_flush()` calls with `flush_on_shutdown()` for depth and candle writers

- [ ] **Item 7: Add depth_dropped_total counter and metric**
  - Files: `crates/storage/src/tick_persistence.rs` (DepthPersistenceWriter)
  - Tests: `test_depth_drop_counter_increments_on_spill_failure`
  - Action: Add u64 counter, increment on disk spill failure, expose via pub getter + Prometheus metric

### Priority 2: Sandbox Enforcement

- [ ] **Item 8: Add sandbox enforcement gate blocking live orders before July 2026**
  - Files: `crates/trading/src/oms/engine.rs`
  - Tests: `test_sandbox_enforcement_blocks_live_before_july`
  - Action: Add runtime check that prevents `dry_run = false` before 2026-07-01

### Priority 3: Verification & Push

- [ ] **Item 9: Run full test suite and verify 0 failures**
  - Files: All crates
  - Tests: `cargo test --workspace`
  - Action: Run, report results, fix any failures

- [ ] **Item 10: Commit all changes + push to branch**
  - Files: All modified files
  - Tests: `cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
  - Action: Commit and push to `claude/check-branch-sync-iBU0b`

## Scenarios

| # | Scenario | Expected (after fixes) |
|---|----------|----------|
| 1 | QuestDB crashes at 9:20 AM, comes back at 10:00 AM | ALL three writers (tick+candle+depth) buffer in ring → spill to disk → auto-drain on reconnect. Zero data loss. |
| 2 | QuestDB never comes back all day | Disk spill captures ALL data types. Recovery on next startup. Zero loss. |
| 3 | App crashes with buffered data | flush_on_shutdown spills ALL writers to disk. recover_stale_spill_files on restart for ALL three. |
| 4 | App restart after crash during QuestDB outage | ALL three writers recover stale spill files at startup. Zero data loss. |
| 5 | Graceful shutdown (Ctrl+C) during QuestDB outage | ALL three writers drain ring buffer → spill to disk. Data safe for next startup. |
| 6 | Someone tries `dry_run = false` before July | Runtime check blocks with clear error message. |

## Files to Modify

| File | Changes |
|------|---------|
| `crates/storage/src/tick_persistence.rs` | Add depth: `flush_on_shutdown()`, `recover_stale_spill_files()`, `depth_dropped_total` counter |
| `crates/storage/src/candle_persistence.rs` | Add `flush_on_shutdown()` method |
| `crates/app/src/main.rs` | Call depth + candle recovery at startup (clippy fix already done) |
| `crates/core/src/pipeline/tick_processor.rs` | Use `flush_on_shutdown()` for depth + candle in shutdown sequence |
| `crates/trading/src/oms/engine.rs` | Add sandbox date enforcement |
