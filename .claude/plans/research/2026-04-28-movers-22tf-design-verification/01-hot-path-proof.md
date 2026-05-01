# Hot-Path O(1) + Zero-Alloc Proof — Movers 22-TF Design

**Branch:** main (post #408 merge)
**Date:** 2026-04-28
**Agent:** hot-path-reviewer

## Verdict Summary

| # | Concern | Verdict |
|---|---|---|
| 1 | Per-tick path stays O(1) zero-alloc | **PASS** |
| 2 | `tokio::time::interval` + `MissedTickBehavior::Skip` + `Arc<MoversTracker>` pattern | **NEEDS-CHANGE** — repo uses `tokio::time::sleep`, no precedent for `interval` |
| 3 | `snapshot_full_universe` iterating 24K HashMap | **RISK** — no proven benchmark; ~1.5 MB/snap × 22 = ~33 MB/s alloc churn |
| 4 | `MoverRow` as Copy struct | **NEEDS-CHANGE** — String fields break Copy; must use ArrayString or intern IDs |
| 5 | Bounded mpsc(8192) drop-oldest | **PASS-with-caveat** — tokio mpsc is drop-NEWEST natively |
| 6 | 22 tasks → 1 writer mpsc | **NEEDS-CHANGE** — fan-in serialization bottleneck; use 22 separate writers |

## §1 PASS — Per-tick path

`top_movers.rs:163-210` `TopMoversTracker::update`:
- Pre-sized HashMap capacity 30000 (line 134)
- Pure float arithmetic
- O(1) `get`/`insert` on composite key `(security_id, segment_code)`
- `SecurityState` is Copy
- No Box/Vec/format!/clone()

Proposed design adds NOTHING to `record_tick`; only adds READER on separate task. Hot-path zero-alloc preserved by construction.

## §2 NEEDS-CHANGE — Scheduler primitive

Repo precedent uses `tokio::time::sleep` (depth_rebalancer.rs:232, connection_pool.rs:266). No code uses `tokio::time::interval`. The plan's choice of `interval` + `MissedTickBehavior::Skip` is correct for deterministic 1s firing, but:

1. Cite `tokio::time::interval` docs as authority (no internal example)
2. Add meta-guard test for `interval` import (no silent regression to `sleep`)
3. **Plan OMITS market-hours gate** per `audit-findings-2026-04-17.md` Rule 3 — would burn ~125M wasted rows/day during off-hours

`Arc<MoversTracker>` shared-reader pattern is sound (precedent: `Arc<FnoUniverse>` at depth_rebalancer.rs:191). BUT MoversTracker `update` takes `&mut self` (line 164) — to share across tasks needs `Arc<RwLock<MoversTracker>>` (which serializes hot path) OR `papaya` per `hot-path.md` mandate. **CONFLICT: plan says HashMap, must be papaya.**

## §3 RISK — 24K HashMap iteration

- Iteration: 24K × ~50 ns ≈ 1.2 ms (plausible)
- Vec<MoverRow> alloc per snapshot: 24K × 64 B ≈ 1.5 MB
- 1Hz × 22 timeframes = **33 MB/sec allocator churn**
- DHAT zero-alloc applies to per-tick path only — snapshot path NOT zero-alloc
- 10ms budget aspirational, no Criterion bench yet

**Required:** caller-owned reusable arena Vec (clear+extend, not new Vec).

## §4 NEEDS-CHANGE — MoverRow Copy

| Field | Type | Bytes |
|---|---|---|
| ts | i64 | 8 |
| security_id | u32 | 4 |
| segment | ? | ? |
| symbol | ? | ? |
| ltp/prev_close/change_pct/change_abs | f64 × 4 | 32 |
| volume/buy_qty/sell_qty | i64 × 3 | 24 |
| received_at | i64 | 8 |

Numeric only = 84 B. With 3 Strings, NOT Copy. Existing `BufferedStockMover` (movers_persistence.rs:283) uses Strings, is Clone NOT Copy.

**Required:** `ArrayString<16>` for symbol/instrument_type, or intern IDs.

For 21-column option variant: ~177 B per row, fits 3 cache lines. 528K row-copies/sec acceptable.

## §5 PASS-with-caveat — mpsc semantics

Tokio `mpsc::Sender::try_send` returns `TrySendError::Full(value)` and drops the **NEWEST** payload, not oldest. Plan says "drop-oldest" — that's not native tokio. To genuinely drop-oldest, receiver must `try_recv` to discard head OR producer must use `tokio::sync::broadcast` (lossy by design) OR custom ring.

Existing `prev_close_writer` uses 64 (tiny). Plan's 8192 may still be small if 22 tasks fire near-simultaneously: 22 × 24K = 528K rows → 8192 holds ~1/3 of a single snapshot.

## §6 NEEDS-CHANGE — Single writer fan-in

22 producers → 1 writer mpsc → single ILP TCP `BufWriter`. At 528K rows/sec peak × 1μs/row append = ~528ms/sec CPU. Writer becomes bottleneck.

**Existing precedent uses ONE writer per concern:** `TickPersistenceWriter`, `StockMoversWriter`, `OptionMoversWriter`, `LiveCandleWriter`, `IndicatorSnapshotWriter`, `DepthDeepWriter`, `PreviousCloseWriter`. **No precedent for 1-writer-multiple-tables.**

**Recommend:** 22 writers, each owning its own mpsc(8192) and ILP `Sender`. Reasons:
1. Failure isolation (matches plan ratchet #4)
2. Backpressure isolation
3. Per-writer reconnect throttle
4. Per-timeframe metric labels align with per-writer ownership

QuestDB sustains >1M rows/sec across multiple ILP connections. 22 connections fine.

## Cross-cutting issues

| # | Issue | Severity |
|---|---|---|
| A | Plan omits `is_within_market_hours_ist()` gate per audit-findings Rule 3 | HIGH |
| B | Plan uses HashMap; hot-path.md mandates papaya for concurrent readers | HIGH |
| C | Ratchet #5 hardcoded 24K — universe varies day-to-day; use per-segment min thresholds | MED |
| D | E2E ratchet doesn't pin IST-midnight first_seen_set reset + multi-task race; needs loom test | MED |
| E | `// O(1) EXEMPT:` comment required on `snapshot_full_universe` for hot-path.md hook | LOW |
| F | DEDUP key syntax — verify against existing STOCK_MOVERS pattern (no explicit `ts`) | LOW |
| G | `Vec<MoverRow>` from snapshot must be `&mut Vec` from arena, not `Vec::new()` | MED |
| H | `live-feed-purity.md` — movers only consume ticks, don't write | PASS |

## Citations

- `crates/core/src/pipeline/top_movers.rs:31, 134, 163-210, 217-300`
- `crates/core/src/pipeline/prev_close_writer.rs:38, 83-110, 119-138`
- `crates/storage/src/movers_persistence.rs:66, 81, 90, 283-322, 334-345`
- `crates/core/src/instrument/depth_rebalancer.rs:41-43, 188-262`
- `crates/core/src/websocket/connection_pool.rs:37-43, 264-294`
- `crates/storage/src/tick_persistence.rs:50, 195-220`
- `.claude/rules/project/audit-findings-2026-04-17.md` Rule 3
- `.claude/rules/project/hot-path.md` (papaya mandate)
