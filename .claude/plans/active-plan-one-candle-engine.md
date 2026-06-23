# Implementation Plan: ONE common candle engine — Groww through the 21-TF MultiTfAggregator (all timeframes, feed=column)

**Status:** APPROVED
**Date:** 2026-06-23
**Approved by:** Parthiban — §45-48 of `active-plan-groww-live-backtest-parity.md` (operator lock 2026-06-22 "from ticks always generate all candle timeframes"; "make everything as common runtime dynamic scalable approach"). This file is the executable design for the SP3 danger-zone aggregator unification.

## Design

### Goal
Groww ticks flow through the SAME `trading::candles::MultiTfAggregator` (21-TF engine) as Dhan, producing ALL 21 timeframes, sealed and written through the SAME `ShadowCandleWriter::append_seal` path, every row tagged `feed`. Delete the Groww-1m-only path (`Groww1mAggregator`, `GrowwCandle1mWriter`/`append_row`).

### The two pre-existing engines (base = 2f6b38b)
- `common::candle_fold::OneMinFoldCell` — a 1-minute-ONLY common cell. Groww uses it via `Groww1mAggregator`. Half-measure to delete (produces only 1m, not 21 TFs).
- `trading::candles::{AggregatorCell, MultiTfAggregator}` — the 21-TF Dhan engine, keyed `(u32 security_id, u8 segment)`, consumes `&ParsedTick`, `LiveCandleState.volume: u64`, seals → `BufferedSeal` → global mpsc → `ShadowCandleWriter::append_seal` → `candles_<tf>` tables. This is THE engine Groww must join.

### Feed-parameterization (FeedStrategy as a typed value, NOT forked code)
1. **late_policy** — Dhan = re-fold 1-bucket-late (`AmendedLate`); Groww = discard. `AggregatorCell::consume_tick` ALREADY implements AmendedLate unconditionally. Add a `FeedStrategy { late_policy: LatePolicy }` arg threaded `MultiTfAggregator::consume_tick` → `AggregatorCell::consume_tick`; under `Discard` the cell skips the amend branch and returns `DiscardLate`. Dhan passes `Refold` (unchanged), Groww `Discard`.
2. **volume width** — `LiveCandleState.volume`/`bucket_start_cumulative` are ALREADY `u64`; `ShadowSealRow` maps via `i64::try_from(u64)`. Groww `cum_volume` i64 ≥ 0 fits u64 losslessly. NO truncation. The CRIT u32 truncation is AVOIDED by NOT routing volume through `ParsedTick.volume: u32` — see override below.
3. **baseline carry** — the cell's `bucket_start_cumulative` is the required per-bucket baseline (threaded by the container's `last_cumulative` atomic). No default.
4. **feed label** — `BufferedSeal` gains `feed: Feed`; `ShadowCandleWriter::append_seal` stamps `seal.feed.as_str()` (was hardcoded `CANDLE_FEED_DHAN`). Dhan = `Feed::Dhan`, Groww = `Feed::Groww`.

### VOLUME width — honest resolution (avoids the u32-truncation CRIT)
`ParsedTick.volume` is `u32`. Groww cum_volume i64 exceeds u32 intraday. Add `cumulative_volume_override: Option<u64>` to `MultiTfAggregator::consume_tick`: Dhan passes `None` (uses `tick.volume`); Groww passes `Some(cum as u64)`. The `last_cumulative` atomic + cell volume are u64 → no truncation. A Groww cum_volume > i64::MAX is impossible (validated `< MAX_PLAUSIBLE_VOLUME`); negative rejected at the bridge. The override is a stack arg (zero-alloc).

### Token width — Groww exchange_token fits u32
`ParsedTick.security_id: u32`. Groww exchange_token MAX observed = 1,175,236 ≪ u32::MAX. The bridge converts `i64 → u32` via `u32::try_from`, REJECTING LOUDLY (error! + skip) on overflow — never a silent `as u32`. Guard test.

### Timestamp — Groww nanos → cell seconds
`ParsedTick.exchange_timestamp: u32` (IST epoch SECONDS). Groww carries IST nanos. The bridge derives `exchange_timestamp = (ts_ist_nanos / 1e9)` and rejects if it overflows u32 (epoch-seconds < 4.29e9 holds until ~2106). Minute bucketing is second-granular; the 1m boundary is identical.

### Groww has no day_open/oi/prev_close
The bridge builds `ParsedTick` with day_open=day_close=0, oi=0. Cell: session_open/prev_day_close stay 0 → pct columns 0 (div-by-zero guard); oi 0. Day-open arming: first bar opens at first-tick LTP (day_open=0 falls back to LTP) — correct (Groww has no auction open). Missing data, NOT forked logic.

### Writer unification (ONE path)
`ShadowCandleWriter` is THE single candle writer. Add `feed` to `BufferedSeal`; `append_seal` stamps `seal.feed.as_str()`. DELETE `GrowwCandle1mWriter`/`append_row`/`GrowwCandle1mRow` from `groww_candle_persistence.rs` (keep `ensure_groww_candles_1m_table` DDL delegation). Groww seals route through the SAME `global_seal_sender()` mpsc → `SealWriterRunner` → `append_seal`.

### groww_bridge rewrite
Bridge stops using `Groww1mAggregator`/`GrowwCandle1mWriter`. It: parses+validates NDJSON (unchanged), persists raw tick to shared `ticks` (unchanged), builds a `ParsedTick` (token u32-checked, ts secs, ltp f32-from-f64, segment code), feeds its OWN Groww `MultiTfAggregator` instance via `consume_tick(&pt, seg, Discard, Some(cum_u64), on_seal)` where `on_seal` pushes `BufferedSeal{feed:Groww}` to `global_seal_sender()`. Groww gets its OWN aggregator INSTANCE (pipeline isolation — engine CODE shared, instance per-feed, so Dhan/Groww never cross-key). Writer is feed-parameterized so the shared mpsc carries both feeds, routed by `seal.feed`.

### Delete `Groww1mAggregator`; keep `candle_fold` (scope bound)
DELETE `aggregator_1m.rs` + its mod line. `common::candle_fold::OneMinFoldCell` is consumed only by that aggregator; deleting it touches `common` (workspace escalation) and is a separate cleanup — KEEP it this PR (dead-but-harmless; its tests document the 1m fold). If pub-fn-wiring guard flags an unused pub fn from it, address at gate time.

### GOLDEN TEST (Step 1 — the safety gate, written + committed FIRST)
Feed a fixed Groww tick sequence (3 ticks one minute + boundary cross + a late tick + i64 cumulative volume) through the NEW path (a Groww `MultiTfAggregator` with Discard) and assert the M1 sealed candle is byte-identical (O/H/L/C, volume i64, seal ts) to the OLD `Groww1mAggregator` output for the same ticks. Also assert the other 20 TFs now produce seals for Groww. If ANY 1m value differs → STOP + report blocker.

## Edge Cases
- Groww token > u32::MAX → bridge rejects loudly (error! + skip), never aliases.
- Groww ts seconds > u32::MAX (post-~2106) → reject; honest documented bound.
- Groww cum_volume i64 ≥ 0 → fits u64 losslessly; negative rejected at bridge.
- All-zero candle (Groww silent) → not false-OK; seal carries real ts; golden asserts real values.
- Late tick under Discard → DiscardLate, counted, never amends (matches old Groww).
- Groww has no day_open/oi → pct/oi columns 0 (missing data, NOT forked logic).
- Pause/resume mid-minute → open bucket unsealed until next tick (same as old; no new loss).

## Failure Modes
- Seal mpsc full → counter + drop (same as Dhan; ring→spill→DLQ downstream).
- QuestDB unreachable → `ShadowCandleWriter` lazy-disconnect, flush Err `error!` (existing).
- Volume override misuse → Dhan None path unchanged; Groww always passes Some.
- Feed mislabel → `BufferedSeal.feed` + append_seal stamp guarantees `feed` column; DEDUP key includes feed.

## Test Plan
- GOLDEN: Groww 1m byte-identical old-vs-new (Step 1).
- Groww 21-TF: other 20 TFs produced for Groww.
- cum-volume i64 through shared cell: large cumulative (>u32::MAX) preserved.
- token-width guard: >u32::MAX rejected loudly.
- feed-tagged seal: append_seal stamps feed=groww/dhan in wire bytes.
- late-discard under Discard: no amend.
- Dhan path UNCHANGED: existing aggregator_cell + multi_tf + shadow_candle_writer tests green (Refold + None override + feed=dhan).
- `cargo test -p tickvault-trading -p tickvault-core -p tickvault-storage`.

## Rollback
Additive + gated: Groww default OFF. Reverting restores the siloed Groww 1m path. Dhan path byte-identical (Refold + None override + Feed::Dhan == old). Shared tables unchanged.

## Observability
Existing seal counters cover both feeds. `feed_health.record_candle(Feed::Groww)` on each Groww seal. Flush failures `error!` with code. Reuses AGGREGATOR-* family — no new error codes.

## Plan Items
- [ ] SP3a — GOLDEN test FIRST.
- [ ] SP3b — `FeedStrategy { late_policy }` + `cumulative_volume_override` threaded through consume_tick. Dhan = Refold + None.
- [ ] SP3c — `BufferedSeal.feed: Feed` + `append_seal` stamps `seal.feed.as_str()`.
- [ ] SP3d — rewrite `groww_bridge.rs` to feed a Groww `MultiTfAggregator` + route seals via `global_seal_sender`; token/ts width guards.
- [ ] SP3e — DELETE `Groww1mAggregator` + mod; DELETE `GrowwCandle1mWriter`/`append_row`/`GrowwCandle1mRow`.

## Guarantee matrix
Cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row). Proof: golden + unit tests; audit via existing `candles_<tf>` DEDUP `(ts,security_id,segment,feed)`; logging via existing seal counters + `error!` flush; performance: per-tick fold O(1)/zero-alloc (override stack arg, FeedStrategy Copy).
