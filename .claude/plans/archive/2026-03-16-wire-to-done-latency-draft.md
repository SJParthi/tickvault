# Implementation Plan: Wire-to-Done Latency Measurement

**Status:** DRAFT
**Date:** 2026-03-16
**Approved by:** pending

## Problem

Currently `received_at` is captured AFTER the mpsc channel hop (tick_processor line 185),
not at WebSocket frame arrival. We cannot measure:
- Channel transit latency (WS read loop → tick processor)
- True wire-to-done latency (WS frame arrival → processing complete)

## Plan Items

- [ ] **1. Add `TimestampedFrame` type in common crate**
  - Files: `crates/common/src/tick_types.rs`
  - Struct: `TimestampedFrame { data: bytes::Bytes, ws_arrived_at: Instant, wall_clock_nanos: i64 }`
  - `ws_arrived_at: Instant` — monotonic clock for latency math (cannot serialize, cannot store)
  - `wall_clock_nanos: i64` — `Utc::now()` at same moment, for QuestDB `received_at` column
  - Must be `Clone` (Bytes is cheaply cloneable, Instant is Copy, i64 is Copy)
  - Zero allocation — Instant is 8 bytes on stack, Bytes is arc-counted
  - Tests: `test_timestamped_frame_is_clone`, `test_timestamped_frame_preserves_instant`

- [ ] **2. Change channel type from `Bytes` to `TimestampedFrame`**
  - Files: `crates/core/src/websocket/connection_pool.rs`
  - Change: `mpsc::channel::<bytes::Bytes>(65536)` → `mpsc::channel::<TimestampedFrame>(65536)`
  - Change: `take_frame_receiver()` return type → `mpsc::Receiver<TimestampedFrame>`
  - Change: `frame_receiver` field type on `WebSocketConnectionPool`
  - Tests: existing pool tests compile

- [ ] **3. Update WebSocketConnection to stamp frames at arrival**
  - Files: `crates/core/src/websocket/connection.rs`
  - Change: `frame_sender` type from `mpsc::Sender<bytes::Bytes>` → `mpsc::Sender<TimestampedFrame>`
  - Change: `new()` parameter type accordingly
  - At line 481 (`Message::Binary(data)` match arm): wrap in `TimestampedFrame { data, ws_arrived_at: Instant::now(), wall_clock_nanos: Utc::now().timestamp_nanos_opt().unwrap_or(0) }`
  - Tests: existing connection tests compile

- [ ] **4. Update tick processor to use carried timestamp + emit 3 histograms**
  - Files: `crates/core/src/pipeline/tick_processor.rs`
  - Change: `run_tick_processor` parameter `frame_receiver: mpsc::Receiver<bytes::Bytes>` → `mpsc::Receiver<TimestampedFrame>`
  - Destructure: `let TimestampedFrame { data: raw_frame, ws_arrived_at, wall_clock_nanos } = frame;`
  - Remove: `let received_at_nanos = chrono::Utc::now()...` at line 185 — replaced by `wall_clock_nanos` from frame
  - Pass `wall_clock_nanos` to `dispatch_frame()` instead of locally computed value
  - Three metrics (all in nanoseconds as f64):
    - `tv_channel_transit_duration_ns` = `tick_start.duration_since(ws_arrived_at).as_nanos()` — time spent in mpsc channel
    - `tv_wire_to_done_duration_ns` = `ws_arrived_at.elapsed().as_nanos()` — recorded at end of tick processing (after existing `tick_start.elapsed()`)
    - Keep existing `tv_tick_processing_duration_ns` = `tick_start.elapsed()` — unchanged
  - Tests: `test_channel_transit_metric_exists`, `test_wire_to_done_metric_exists`

- [ ] **5. Build verification**
  - Run `cargo check --workspace` — all types align
  - Run `cargo test -p tickvault-common -p tickvault-core -p tickvault-storage`
  - All existing tests pass + new tests pass

## Architecture After Change

```
[Dhan Server] ──network──▶ [WS read loop]
                                │
                                ├─ ws_arrived_at = Instant::now()      ← NEW: monotonic
                                ├─ wall_clock_nanos = Utc::now()       ← NEW: wall clock (moved here from tick_processor)
                                ▼
                           TimestampedFrame { data, ws_arrived_at, wall_clock_nanos }
                                │
                                ▼  mpsc channel (65536 buffer)
                                │
                           tick_processor receives frame
                                │
                                ├─ tick_start = Instant::now()         ← existing
                                ├─ channel_transit = tick_start - ws_arrived_at  ← NEW metric
                                │
                                ▼  parse → dedup → store → broadcast
                                │
                                ├─ processing_duration = tick_start.elapsed()    ← existing metric
                                └─ wire_to_done = ws_arrived_at.elapsed()        ← NEW metric
```

## Three Latency Metrics in Grafana

| Metric | What it measures | Expected range |
|--------|-----------------|----------------|
| `tv_channel_transit_duration_ns` | Time in mpsc channel (WS → tick processor) | 1–10 μs normal, spikes = backpressure |
| `tv_tick_processing_duration_ns` | Parse + dedup + store + broadcast | 0.5–5 μs (existing) |
| `tv_wire_to_done_duration_ns` | Total: channel transit + processing | 2–15 μs normal |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Normal tick flow | channel_transit ~1-10μs, wire_to_done ~5-50μs |
| 2 | Tick processor backpressure | channel_transit spikes visible in Grafana |
| 3 | Multiple WS connections | Each stamps independently, all flow to same receiver |
| 4 | Reconnection | New frames get fresh Instants, no stale timestamps |
| 5 | `received_at` in QuestDB | Now reflects WS arrival time (more accurate than before) |
