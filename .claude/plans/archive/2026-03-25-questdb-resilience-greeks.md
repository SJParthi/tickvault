# Implementation Plan: QuestDB Resilience + Per-Tick Greeks as Columns

**Status:** IN_PROGRESS
**Date:** 2026-03-25
**Approved by:** Parthiban

## Problem Statement

Two interleaved issues:
1. **Resilience:** QuestDB disconnection cascades into app instability. Candle/Greeks writers have zero resilience (no reconnect, no buffer). `break` on errors silently drops data. `std::thread::sleep()` blocks the async executor for 4 seconds during reconnect.
2. **Greeks:** Greeks are stored in separate tables requiring JOINs. Need to be computed per-tick and stored as columns directly in `ticks` and `candles_1s` tables.

Both touch the same files (tick_processor.rs, candle_persistence.rs, greeks_persistence.rs, main.rs). Doing resilience first provides a crash-proof foundation for Greeks.

## Plan Items

### Phase A: Critical Resilience Fixes (prevent crashes)

- [x] **A1.** Fix tick_processor.rs candle loop: `break` → `continue` on append error (line 655)
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Add rate-limited error counter (warn first 100, then 1 per 1000)
  - Tests: test_candle_write_error_continues_remaining_candles

- [x] **A2.** Fix main.rs greeks loops: `break` → `continue` on write errors (lines 1947, 1983)
  - Files: crates/app/src/main.rs
  - Add rate-limited error counter
  - Tests: (integration — tested via pipeline; break→continue is 1-line change)

- [x] **A3.** Fix `std::thread::sleep()` in reconnect — must not block async executor
  - Files: crates/storage/src/tick_persistence.rs (lines 330, 743)
  - Replace `std::thread::sleep()` with `tokio::task::block_in_place()` wrapper OR move reconnect to `tokio::spawn_blocking()` — keeps reconnect off the hot path async task
  - For tick_processor (sync context): keep thread::sleep but gate reconnect attempts to max 1 per 30 seconds (not per tick)
  - Add `last_reconnect_attempt: Instant` field to prevent reconnect storms
  - Tests: test_reconnect_throttled_to_once_per_30s

- [x] **A4.** Fix token expiry check in `TokenHandleBridge::get_access_token()`
  - Files: crates/app/src/trading_pipeline.rs (lines 59-70)
  - Add `if !token_state.is_valid() { return Err(OmsError::TokenExpired); }`
  - Tests: test_token_bridge_rejects_expired_token

### Phase B: LiveCandleWriter Resilience (mirror TickPersistenceWriter)

- [x] **B1.** Change `sender: Sender` → `sender: Option<Sender>` + store `ilp_conf_string`
  - Files: crates/storage/src/candle_persistence.rs (LiveCandleWriter struct)
  - Tests: test_live_candle_writer_option_sender

- [x] **B2.** Add `reconnect()` with exponential backoff (1s/2s/4s, 3 attempts) + throttle
  - Files: crates/storage/src/candle_persistence.rs
  - Add `last_reconnect_attempt: Instant` — throttle to max 1 attempt per 30s
  - Tests: test_live_candle_reconnect_throttled

- [x] **B3.** Add bounded ring buffer `VecDeque<BufferedCandle>` for candle buffering
  - Files: crates/storage/src/candle_persistence.rs, crates/common/src/constants.rs
  - New struct `BufferedCandle` (copy of append_candle args, ~48 bytes)
  - New constant: `CANDLE_BUFFER_CAPACITY = 100_000` (~5MB, holds ~27 min at 60 candles/sec)
  - `buffer_candle()` — push to ring buffer, drop oldest when full + CRITICAL alert per 1000
  - `drain_candle_buffer()` — drain to QuestDB after recovery, batched
  - Tests: test_candle_ring_buffer_fifo, test_candle_ring_buffer_overflow_drops_oldest, test_candle_drain_after_recovery

- [x] **B4.** Make `append_candle()` and `force_flush()` resilient (swallow errors, buffer candles)
  - Files: crates/storage/src/candle_persistence.rs
  - `append_candle()`: if sender None → try reconnect (throttled) → buffer on failure → always return Ok(())
  - `force_flush()`: on flush error → set sender=None → don't propagate → buffer remaining
  - Tests: test_live_candle_append_ok_when_disconnected, test_live_candle_flush_error_nulls_sender

### Phase C: GreeksPersistenceWriter Resilience

- [x] **C1.** Change `sender: Sender` → `sender: Option<Sender>` + reconnect + throttle
  - Files: crates/storage/src/greeks_persistence.rs
  - Add `ilp_conf_string: String`, `last_reconnect_attempt: Instant`
  - Add `reconnect()` with exponential backoff (same as tick writer)
  - No ring buffer needed — Greeks recomputed every second from live ticks
  - Tests: test_greeks_writer_option_sender, test_greeks_reconnect_throttled

- [x] **C2.** Make all `write_*_row()` and `flush()` resilient
  - Files: crates/storage/src/greeks_persistence.rs
  - Try reconnect (throttled) if sender None → on failure skip write → return Ok
  - `flush()`: on error → set sender=None → return Ok (don't propagate)
  - Tests: test_greeks_write_ok_when_disconnected, test_greeks_flush_error_nulls_sender

### Phase D: CandlePersistenceWriter (historical) Resilience

- [x] **D1.** Change `sender: Sender` → `sender: Option<Sender>` + reconnect
  - Files: crates/storage/src/candle_persistence.rs (CandlePersistenceWriter)
  - Cold path — propagate errors (caller decides to retry or skip)
  - Tests: test_historical_candle_writer_reconnect

### Phase E: Disk Spill for All-Day QuestDB Outage

- [ ] **E1.** Add disk spill to TickPersistenceWriter when ring buffer full
  - Files: crates/storage/src/tick_persistence.rs, crates/common/src/constants.rs
  - When ring buffer at TICK_BUFFER_CAPACITY and still disconnected:
    - Open append-only binary file: `data/spill/ticks-{YYYYMMDD}.bin`
    - Write fixed-size records (ParsedTick serialized, ~80 bytes each)
    - O(1) amortized (buffered sequential append)
  - New constants: `TICK_SPILL_DIR`, `TICK_SPILL_ENABLED` (default true)
  - Tests: test_disk_spill_write_read_roundtrip, test_disk_spill_oldest_first_recovery

- [ ] **E2.** Add disk spill drain on recovery
  - Files: crates/storage/src/tick_persistence.rs
  - On reconnect: drain ring buffer → drain disk spill → delete spill file
  - Batched reads (TICK_FLUSH_BATCH_SIZE at a time)
  - Tests: test_disk_spill_drain_deletes_file, test_recovery_order_ringbuf_then_disk

- [ ] **E3.** Add disk spill to LiveCandleWriter when ring buffer full
  - Files: crates/storage/src/candle_persistence.rs
  - Same pattern: `data/spill/candles-{YYYYMMDD}.bin`
  - BufferedCandle is ~48 bytes, fixed format
  - Tests: test_candle_disk_spill_roundtrip, test_candle_disk_spill_drain

### Phase F: Metrics & Observability for Resilience

- [ ] **F1.** Add Prometheus metrics for resilience state
  - Files: crates/storage/src/candle_persistence.rs, crates/storage/src/tick_persistence.rs, crates/storage/src/greeks_persistence.rs, crates/common/src/constants.rs
  - `tv_candle_buffer_size` — candle ring buffer occupancy
  - `tv_candles_dropped_total` — candles dropped from buffer overflow
  - `tv_greeks_write_errors_total` — greeks write failures
  - `tv_disk_spill_bytes_written` — bytes written to disk spill
  - `tv_disk_spill_records_drained` — records drained from disk spill
  - `tv_reconnect_attempts_total` — total reconnect attempts per writer
  - Tests: (metrics emission verified in integration tests)

### Phase G: Per-Tick Greeks Implementation

- [ ] **G1.** Add 6 Greeks fields to `ParsedTick` struct (Item 1 — REDO)
  - Files: crates/common/src/tick_types.rs
  - Add `iv: f64`, `delta: f64`, `gamma: f64`, `theta: f64`, `vega: f64`, `rho: f64`
  - Default to `f64::NAN` (QuestDB NULL) for non-F&O ticks
  - Tests: test_parsed_tick_greeks_default_nan, test_parsed_tick_greeks_copy

- [ ] **G2.** Add 6 Greeks columns to `ticks` table DDL + `build_tick_row()`
  - Files: crates/storage/src/tick_persistence.rs
  - Add `iv DOUBLE, delta DOUBLE, gamma DOUBLE, theta DOUBLE, vega DOUBLE, rho DOUBLE` to DDL
  - Add 6 `.column_f64()` calls in `build_tick_row()`
  - NaN → QuestDB NULL (no storage cost)
  - Tests: test_ticks_ddl_has_greeks_columns, test_build_tick_row_writes_greeks

- [ ] **G3.** Add 6 Greeks columns to `candles_1s` DDL
  - Files: crates/storage/src/materialized_views.rs
  - Tests: test_candles_1s_ddl_has_greeks_columns

- [ ] **G4.** Add `last(iv)`, `last(delta)`, etc. to all 18 materialized views
  - Files: crates/storage/src/materialized_views.rs
  - Modify `build_view_sql()` SELECT clause
  - Tests: test_build_view_sql_has_greeks_columns, test_all_18_views_have_greeks

- [ ] **G5.** Add Greeks to CompletedCandle + LiveCandleWriter::append_candle()
  - Files: crates/core/src/pipeline/candle_aggregator.rs, crates/storage/src/candle_persistence.rs
  - Add 6 f64 fields to CompletedCandle
  - Modify `append_candle()` to write 6 Greeks columns
  - Tests: test_completed_candle_has_greeks, test_live_candle_writes_greeks

- [ ] **G6.** Track last Greeks per instrument in candle aggregator
  - Files: crates/core/src/pipeline/candle_aggregator.rs
  - On F&O tick with valid Greeks: store last (iv, delta, gamma, theta, vega, rho) per security_id
  - On candle close: populate CompletedCandle Greeks from cache
  - Tests: test_aggregator_tracks_last_greeks, test_candle_close_carries_greeks

- [ ] **G7.** Repurpose GreeksAggregator for inline tick computation
  - Files: crates/trading/src/greeks/aggregator.rs
  - Remove: `GreeksEmission`, candle-close emission
  - Keep: underlying LTP cache, option state tracking
  - Add: `compute_for_tick(&mut self, tick: &ParsedTick, registry: &InstrumentRegistry) -> GreeksResult`
  - Returns: (iv, delta, gamma, theta, vega, rho) or NaN tuple
  - Tests: test_compute_for_tick_fno_returns_greeks, test_compute_for_tick_equity_returns_nan

- [ ] **G8.** Compute Greeks inline in tick processor
  - Files: crates/core/src/pipeline/tick_processor.rs
  - For NSE_FNO ticks: call `aggregator.compute_for_tick()` → populate tick.iv/delta/etc.
  - For non-F&O: leave as NaN
  - Wire GreeksAggregator + InstrumentRegistry into tick processor params
  - Tests: test_tick_processor_populates_greeks_fno, test_tick_processor_nan_greeks_equity

- [ ] **G9.** Remove `option_greeks_live` table + code
  - Files: crates/storage/src/greeks_persistence.rs
  - Remove: DDL, constant, dedup key, OptionGreeksLiveRow struct, write method
  - Tests: test_no_option_greeks_live_references

- [ ] **G10.** Remove `pcr_snapshots_live` table + code
  - Files: crates/storage/src/greeks_persistence.rs
  - Remove: DDL, constant, dedup key, PcrSnapshotLiveRow struct, write method
  - Tests: test_no_pcr_snapshots_live_references

- [ ] **G11.** Remove `run_greeks_aggregator_consumer()` spawn from main.rs
  - Files: crates/app/src/main.rs
  - Remove spawns at lines ~547 and ~1026
  - Keep `run_greeks_pipeline()` (periodic API cross-verification)
  - Tests: compile check (dead code removal)

- [ ] **G12.** Update all existing tests for schema changes
  - Files: crates/storage/src/tick_persistence.rs (inline tests), crates/storage/src/materialized_views.rs (inline tests), crates/core/src/pipeline/candle_aggregator.rs (inline tests), crates/common/tests/schema_validation.rs, crates/storage/tests/grafana_query_tests.rs
  - Every ParsedTick/CompletedCandle instantiation: add Greeks fields
  - DDL assertions: verify Greeks columns
  - Tests: all existing tests pass with new schema

### Phase H: Comprehensive Test Suite

- [ ] **H1.** Storage resilience integration tests
  - Files: crates/storage/tests/resilience_integration.rs (NEW)
  - Use TCP drain server to simulate QuestDB disconnect
  - Tests: test_tick_writer_survives_disconnect, test_candle_writer_survives_disconnect, test_greeks_writer_survives_disconnect, test_sustained_failure_10k_ticks_buffered

- [ ] **H2.** DHAT zero-allocation test for candle hot path
  - Files: crates/storage/tests/dhat_candle_persistence.rs (NEW)
  - Verify `append_candle()` on happy path does zero heap alloc
  - Tests: dhat_live_candle_append_zero_alloc

- [ ] **H3.** Tick processor resilience tests
  - Files: crates/core/src/pipeline/tick_processor.rs (inline tests)
  - Test: candle write error → remaining candles still processed
  - Test: QuestDB disconnect → ticks still flow through pipeline
  - Tests: test_candle_error_does_not_break_loop, test_pipeline_continues_without_questdb

- [ ] **H4.** Disk spill unit tests
  - Files: crates/storage/src/tick_persistence.rs (inline tests)
  - Tests: test_spill_write_read_roundtrip, test_spill_file_cleanup, test_recovery_order

- [ ] **H5.** Greeks pipeline integration tests
  - Files: crates/core/tests/greeks_inline_e2e.rs (NEW)
  - End-to-end: F&O tick → Greeks computed → tick persisted with Greeks → candle has Greeks
  - Tests: test_fno_tick_has_greeks_in_persistence, test_equity_tick_has_nan_greeks

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB down at startup | Writers init with sender=None, buffer data, app runs, trading continues |
| 2 | QuestDB disconnects mid-day | Writers detect error, buffer, reconnect with backoff (throttled 30s) |
| 3 | QuestDB down entire day | Ring buffer + disk spill, zero tick loss, drain on recovery |
| 4 | QuestDB recovers | Ring buffer → disk spill → QuestDB, dedup keys prevent duplicates |
| 5 | Candle write fails | tick_processor continues remaining candles (no break) |
| 6 | Greeks write fails | aggregator continues remaining snapshots (no break) |
| 7 | Token expires mid-session | TokenHandleBridge returns Err(TokenExpired), not stale token |
| 8 | Dhan sends unknown packet | Parser logs warn + skips (no panic) |
| 9 | F&O tick arrives with spot | Greeks computed, 6 columns in tick + candle |
| 10 | Equity tick arrives | Greeks = NaN, zero compute overhead |
| 11 | Disk full (can't spill) | Log CRITICAL, drop oldest from ring buffer |
| 12 | All views queried | Greeks columns available via last() aggregation |
| 13 | QuestDB broken pipe mid-flush (Greeks) | Fresh buffer created (not clear()), next cycle writes+flushes normally |
| 14 | Dhan option chain API 502/500 | Retry 3x with 1s/2s/4s backoff, then skip |
| 15 | Dhan 502 for 5+ consecutive cycles | Escalate to ERROR (Telegram alert) |

### Phase I: Greeks Persistence Bug Fixes (Live Production Bugs)

- [ ] **I1.** Fix buffer corruption in flush/reconnect: replace buffer.clear() with Buffer::new()
  - Files: crates/storage/src/greeks_persistence.rs
  - Tests: test_flush_reconnect_fresh_buffer, test_flush_error_fresh_buffer

- [ ] **I2.** Add retry with exponential backoff for Dhan option chain 502/500
  - Files: crates/core/src/option_chain/client.rs
  - Tests: test_fetch_expiry_list_retries_on_server_error, test_fetch_option_chain_retries_on_server_error

- [ ] **I3.** Add consecutive failure counter + ERROR escalation in greeks pipeline
  - Files: crates/app/src/greeks_pipeline.rs
  - Tests: (verified via log escalation pattern in pipeline loop)
