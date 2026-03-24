# Implementation Plan: Tick-Driven Greeks Pipeline with Candle-Aligned Timestamps

**Status:** IN_PROGRESS
**Date:** 2026-03-24
**Approved by:** Parthiban

## Problem

Greeks/PCR snapshots currently use `Utc::now()` as timestamp (system clock from REST API fetch time).
This cannot be mapped to ticks/candles which use `exchange_timestamp` (NSE clock).

**Critical requirement:** When the strategy evaluator checks a candle at 09:15:00 and asks
"what were the Greeks at this moment?", the answer must be a Greeks row with `ts = 09:15:00`.
The trading decision (ENTER/EXIT) depends on candle + Greeks + PCR all sharing the EXACT same timestamp.

## Approach

**Greeks/PCR snapshots are emitted at candle close boundaries**, not per-tick.
The `GreeksAggregator` continuously updates its internal state from every tick,
but only **emits a snapshot when a candle completes** — stamped with the candle's timestamp.

This means:
- Ticks update internal state (latest option LTP, latest spot, latest OI)
- When candle aggregator emits a completed 1s/1m candle → GreeksAggregator emits Greeks snapshot
- Greeks snapshot `ts` = candle `ts` = exact match
- Strategy evaluator sees: `candle OHLCV + Greeks + PCR` all at same `ts`

```
Strategy decision at ts=09:15:00:

  candles_1m:          ts=09:15:00  open=22950 high=22965 low=22940 close=22958
  option_greeks_live:  ts=09:15:00  delta=-0.464  gamma=0.00378  iv=18.29  theta=-88.27
  pcr_snapshots_live:  ts=09:15:00  pcr_oi=0.95  sentiment=Bearish

  → All three rows share ts=09:15:00
  → Strategy evaluates: IF delta > -0.5 AND pcr < 1.0 → ENTER SHORT PUT
```

## Plan Items

- [x] 1. Add underlying LTP cache to track index/equity spot prices from ticks
  - Files: crates/trading/src/greeks/aggregator.rs (NEW)
  - Pattern: `HashMap<u32, (f32, u32)>` mapping security_id → (latest_ltp, exchange_timestamp)
  - Updated on every IDX_I/NSE_EQ tick; read when computing F&O Greeks
  - Tests: test_underlying_ltp_cache_update, test_underlying_ltp_cache_miss_skips_greeks

- [x] 2. Add underlying security_id reverse lookup (symbol → security_id)
  - Files: crates/common/src/instrument_registry.rs
  - Add `fn get_underlying_security_id(&self, symbol: &str) -> Option<u32>` method
  - Build reverse map at registry construction time (cold path, O(N) once)
  - Tests: test_underlying_reverse_lookup, test_underlying_reverse_lookup_missing

- [x] 3. Create `GreeksAggregator` — candle-aligned Greeks computation engine
  - Files: crates/trading/src/greeks/aggregator.rs (NEW)
  - **Two-phase design:**
    - Phase A (per-tick, O(1)): Update internal state maps:
      - `option_state: HashMap<u32, OptionTickState>` (LTP, OI, volume per F&O security_id)
      - `underlying_ltp: HashMap<u32, f32>` (spot price per underlying)
    - Phase B (on candle close): Compute Greeks for ALL tracked options, emit snapshots
      - Triggered by candle aggregator signal (candle_ts notification)
      - Iterates option_state map, computes BS Greeks for each
      - Emits rows with `ts = candle_ts` (the candle boundary)
      - Computes PCR per underlying/expiry from aggregated OI
  - O(1) per tick update, O(N) per candle close (N = active options, ~2-5K)
  - Tests: test_greeks_aggregator_updates_state_on_tick,
           test_greeks_aggregator_emits_on_candle_close,
           test_greeks_aggregator_timestamp_matches_candle,
           test_greeks_aggregator_skips_when_no_underlying_ltp

- [x] 4. Create `option_greeks_live` QuestDB table
  - Files: crates/storage/src/greeks_persistence.rs
  - Same columns as `option_greeks` plus `candle_interval` (SYMBOL: "1s", "1m", etc.)
  - DEDUP KEY: `(ts, security_id, segment, candle_interval)`
  - `ts` = candle boundary timestamp (exchange clock, IST)
  - Keep existing `option_greeks` for Dhan API audit trail
  - Tests: test_option_greeks_live_ddl, test_dedup_key_includes_candle_interval

- [x] 5. Create `pcr_snapshots_live` QuestDB table
  - Files: crates/storage/src/greeks_persistence.rs
  - Same columns as `pcr_snapshots` plus `candle_interval`
  - DEDUP KEY: `(ts, underlying_symbol, expiry_date, candle_interval)`
  - `ts` = candle boundary timestamp
  - Tests: test_pcr_snapshots_live_ddl

- [x] 6. Candle-close detection via tick second boundary
  - Files: crates/app/src/main.rs (run_greeks_aggregator_consumer)
  - Approach: detect 1-second boundary from tick.exchange_timestamp changes
  - When exchange_timestamp second changes → snapshot previous second
  - No separate mpsc channel needed — simpler, same result
  - Tests: covered by aggregator unit tests (test_greeks_aggregator_emits_on_candle_close)

- [x] 7. Wire GreeksAggregator into boot sequence
  - Files: crates/app/src/main.rs
  - Spawned as background tokio task in BOTH fast boot and slow boot paths
  - Subscribes to tick_broadcast (same pattern as candle persistence consumer)
  - Writes to QuestDB via GreeksPersistenceWriter
  - Gated on config.greeks.enabled + subscription_plan availability
  - Tests: TEST-EXEMPT (requires live QuestDB + tick broadcast)

## Architecture

```
WebSocket (Full mode, 162 bytes)
    │
    ▼
Tick Processor (parse binary → ParsedTick)
    │
    ├─► Tick Persistence (QuestDB: ticks table, ts=exchange_timestamp)
    │
    ├─► Broadcast Channel (ParsedTick)
    │       │
    │       ├─► Candle Aggregator
    │       │       │
    │       │       ├─► Candle Persistence (QuestDB: candles, ts=candle_open)
    │       │       │
    │       │       └─► CandleCloseEvent channel ──┐
    │       │                                       │
    │       ├─► Strategy Evaluator                  │
    │       │                                       │
    │       └─► GreeksAggregator (NEW)  ◄───────────┘
    │               │
    │               ├─ Per tick: update internal state (O(1))
    │               │   - IDX_I tick → underlying_ltp[sec_id] = ltp
    │               │   - NSE_FNO tick → option_state[sec_id] = {ltp, oi, vol}
    │               │
    │               └─ On CandleCloseEvent(candle_ts):
    │                   - For each option in option_state:
    │                       compute_greeks(underlying_ltp, strike, T, r, q, option_ltp)
    │                   - Aggregate OI → compute PCR per underlying
    │                   - Write to QuestDB:
    │                       option_greeks_live.ts = candle_ts
    │                       pcr_snapshots_live.ts = candle_ts
    │
    └─► [Existing] Greeks Pipeline (REST API, every 60s, audit only)
```

## Timestamp Alignment (the whole point)

```
09:15:00 ─── candle boundary ───────────────────── 09:16:00
   │                                                   │
   │  ticks: 09:15:01  15:02  15:03  ...  15:58  15:59│
   │         ↓ updates state    ↓ updates state        │
   │                                                   │
   │  candle_aggregator completes 1m candle:           │
   │  → CandleCloseEvent { ts: 09:15:00 }             │
   │                                                   │
   │  greeks_aggregator receives event:                │
   │  → snapshot Greeks using LATEST tick state        │
   │  → ts = 09:15:00 (same as candle)                │
   │                                                   │
   │  QuestDB rows:                                    │
   │    candles_1m.ts        = 09:15:00 ✓              │
   │    option_greeks_live.ts = 09:15:00 ✓             │
   │    pcr_snapshots_live.ts = 09:15:00 ✓             │
   │                                                   │
   │  Strategy evaluator at 09:16:00:                  │
   │    SELECT c.close, g.delta, p.pcr_oi              │
   │    FROM candles_1m c                              │
   │    JOIN option_greeks_live g ON c.ts = g.ts       │
   │    JOIN pcr_snapshots_live p ON c.ts = p.ts       │
   │    WHERE c.ts = '09:15:00'                        │
   │    → EXACT MATCH, no approximation ✓              │
```

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 1m candle closes at 09:15:00 | Greeks/PCR snapshot emitted with ts=09:15:00 |
| 2 | F&O tick arrives, no underlying LTP yet | State updated but no Greeks on candle close for that option |
| 3 | IDX_I tick arrives | Updates underlying_ltp cache only |
| 4 | Multiple candle intervals (1s, 1m) configured | Separate snapshot per interval, each with matching ts |
| 5 | Expiry day, T approaching 0 | MIN_TIME_TO_EXPIRY guard in BS code handles it |
| 6 | Strategy asks: "delta at 09:15:00?" | Exact JOIN on ts=09:15:00 → precise answer |
| 7 | No ticks during a candle period | Candle still closes, Greeks use last known state |
| 8 | OI changes mid-candle | Internal state updated; PCR snapshot at candle close reflects latest |
