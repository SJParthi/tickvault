# Movers 22-TF v2 — Architecture (with 6 NEEDS-CHANGE fixes baked in)

**Status:** DRAFT v2 (pending Parthiban approval)
**Date:** 2026-04-28
**Branch:** `claude/new-session-TBQe7`
**Supersedes:** `active-plan-movers-22tf-redesign.md` (v1)

## What changed vs v1

| # | v1 design | v2 fix (audit-driven) | Source |
|---|---|---|---|
| 1 | `Arc<MoversTracker>` with `&mut self` HashMap | **`papaya::HashMap<(u32, ExchangeSegment), SecurityState>`** for the per-instrument state — lock-free concurrent readers per `hot-path.md` | 01 §2 + cross-cutting B |
| 2 | 1 writer (`MoversWriter22`) for all 22 tables | **22 writers** — one per timeframe, each owning its own `mpsc(8192)` + ILP `Sender` for failure + backpressure isolation | 01 §6 |
| 3 | `MoverRow` with `String` symbol fields → not `Copy` | **`ArrayString<16>` for `symbol` + `instrument_type`**; struct is `Copy`, ≤192 B | 01 §4 |
| 4 | Scheduler runs 24/7 | **Market-hours gate** — `is_within_market_hours_ist()` short-circuits `tokio::interval` ticks outside `[09:00, 15:30]` IST | 01 §A + audit-findings Rule 3 |
| 5 | `Vec<MoverRow>` allocated each snapshot | **Caller-owned arena `Vec<MoverRow>`** — `clear() + extend()` per snap, capacity pre-sized to 30,000 | 01 §3 + §G |
| 6 | mpsc “drop-oldest” claimed | **drop-NEWEST** semantics (native tokio) explicitly documented; `tv_movers_writer_dropped_total{reason="full_drop_newest"}` + 0.1% SLA alert | 01 §5 |

## Components — 7 modules (~2,400 LoC, was 1,800)

| # | Module | Path | Purpose |
|---|---|---|---|
| 1 | `MoverRow` | `crates/common/src/mover_types.rs` | Copy struct, `ArrayString<16>` symbol fields, ≤192 B |
| 2 | `MoversTracker` (extend) | `crates/core/src/pipeline/top_movers.rs` | Replace `HashMap` with `papaya::HashMap`; add `snapshot_into(&mut Vec<MoverRow>)` arena method |
| 3 | `Movers22TfWriter` (×22 instances) | `crates/storage/src/movers_22tf_persistence.rs` | One writer per timeframe; owns mpsc(8192) + ILP `Sender`; reuses `BufferedStockMover` ILP idiom |
| 4 | `MoversSnapshotScheduler` | `crates/core/src/pipeline/movers_22tf_scheduler.rs` | 22 tasks via `tokio::time::interval` + `MissedTickBehavior::Skip`; market-hours gate; arena per task |
| 5 | `Movers22TfWriterState` | `crates/core/src/pipeline/movers_22tf_writer_state.rs` | `OnceLock<[Sender<MoverRow>; 22]>`; mirrors `prev_close_writer.rs` |
| 6 | `MoversSupervisor` | `crates/core/src/pipeline/movers_22tf_supervisor.rs` | Joins 22 `JoinHandle`s; respawns within 5s on panic; emits `MOVERS-22TF-02` |
| 7 | 9 candle materialized views | `crates/storage/src/materialized_views.rs` | `candles_4m..14m` (skip 5/10/15) sourced from `candles_1m` |

## Hot-path latency — O(1), zero-alloc (mechanical, not aspirational)

Per-tick path (preserved):
1. `papaya::HashMap::get` on composite `(u32, ExchangeSegment)` — **O(1) lock-free**
2. `update_prev_close` — `papaya::HashMap::pin().insert` — **O(1)**
3. `change_pct` arithmetic — pure float — **O(1)**

Snapshot path (off hot path, on cold task):
- `iter()` over papaya — O(N=24K) ~ 1.2 ms
- Arena `Vec::clear()` + `extend()` — zero alloc after first call
- 22 mpsc `try_send` (drop-newest) — O(1) per row

DHAT pin: `dhat_movers_22tf_record_tick.rs` → `total_blocks == 0` over 10K ticks.
Criterion: `movers_record_tick ≤ 50ns p99`, `movers_snapshot_24k ≤ 10ms`.

## Prev_close source per segment (UNCHANGED — verified by 4-agent audit)

| Segment | Source | Path |
|---|---|---|
| IDX_I (~30) | PrevClose code-6 packet → in-memory cache + `previous_close` table | `tick_processor.rs:1612` |
| NSE_EQ (~216) | `tick.day_close` field bytes 38-41 (Quote) or 50-53 (Full) | `top_movers.rs:185` |
| NSE_FNO index F&O (~2,067) | `tick.day_close` bytes 50-53 (Full) | same |
| NSE_FNO stock F&O (~22,011) | `tick.day_close` bytes 50-53 (Full) | same |

## 22 movers tables — DDL (unchanged from v1, capacity math verified)

```sql
CREATE TABLE IF NOT EXISTS movers_{T} (
    ts                  TIMESTAMP,
    security_id         INT,
    segment             SYMBOL CAPACITY 16 NOCACHE,
    underlying_symbol   SYMBOL CAPACITY 1024 CACHE,
    instrument_type     SYMBOL CAPACITY 32 NOCACHE,
    ltp                 DOUBLE,
    prev_close          DOUBLE,
    change_pct          DOUBLE,
    change_abs          DOUBLE,
    volume              LONG,
    buy_qty             LONG,
    sell_qty            LONG,
    received_at         TIMESTAMP
) timestamp(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, security_id, segment);
```

Total: ~125M rows/day, ~17GB/90d (fits 100GB EBS hot tier per `aws-budget.md`).

## Why 22 writers, not 1

Single-writer design from v1 forces all 22 timeframes through ONE TCP connection.
Peak (all 22 fire same 60s wall-clock boundary): **528K rows/sec** through one
ILP `BufWriter`. QuestDB `QDB_LINE_TCP_NET_ACTIVE_CONNECTION_LIMIT=20` allows
22 connections (need 1 bump or share writer pool of 11×2). 22 writers also:
- Failure isolation (one TCP RST does not poison all 22)
- Per-timeframe Prometheus labels align with per-writer ownership
- Each writer reconnects independently with its own backoff
- Matches existing precedent (`TickPersistenceWriter`, `LiveCandleWriter`,
  `IndicatorSnapshotWriter`, `DepthDeepWriter`, `PreviousCloseWriter` —
  always 1-writer-per-concern, NEVER 1-writer-many-tables)

## Files to read

- Risks: `.claude/plans/v2-risks.md`
- Ratchets: `.claude/plans/v2-ratchets.md`
- Phased commits: `.claude/plans/v2-phases.md`
- Index: `.claude/plans/active-plan-movers-22tf-redesign-v2.md`
