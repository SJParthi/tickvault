# Movers 22-TF v3 — Architecture (final scope: movers + Dhan UI + depth + expiry rollover)

**Status:** DRAFT v3 (pending Parthiban approval)
**Date:** 2026-04-28
**Branch:** `claude/new-session-TBQe7`
**Supersedes:** v2 (which superseded v1)

## What v3 changes vs v2

| Area | v2 → v3 |
|---|---|
| Categorization | Single `segment` column → **`segment` + `instrument_type`** (4-way: index/stock/option/future) |
| MoverRow columns | 13 → **26** (added 4 derivative-meta + 4 OI/spot + 5 depth) |
| MoverRow size | ~192 B → **~296 B** (still `Copy`, 5 cache lines) |
| UI tabs | placeholder → **Stocks 2 + Index 2 + Options 7 + Futures 7 + Depth 6 = 24 panels** |
| Stocks scope | unspecified → **Price Movers (Gainers/Losers) only** (per Parthiban call) |
| Expiry-month dropdown | not designed → **populated from `derivative_contracts.expiry_date`** |
| Depth integration | OUT OF SCOPE → **IN SCOPE** (Option A: columns on movers + Option C: 6 dashboard panels) |
| Stock F&O expiry rollover | T-1 (existing rule) → **T-only** (Dhan disallows expiry-day trading) |
| Ratchets | 37 → **47** |
| Implementation phases | 6 commits → **7** (added expiry rollover commit) |

## 6 architectural fixes (preserved from v2)

| # | Fix | Why |
|---|---|---|
| 1 | `papaya::HashMap<(u32, ExchangeSegment), SecurityState>` | Lock-free concurrent readers per `hot-path.md` |
| 2 | 22 isolated ILP writers (one per timeframe) | Failure + backpressure isolation; matches existing precedent |
| 3 | `ArrayString<16>` for symbol fields → `MoverRow: Copy` | Zero-alloc snapshot serialization |
| 4 | Market-hours gate (`is_within_market_hours_ist`) | No off-hours waste, audit-findings Rule 3 |
| 5 | Caller-owned arena `Vec<MoverRow>` per writer | `clear() + extend()`, zero per-snap alloc |
| 6 | Drop-NEWEST mpsc semantics + 0.1% SLA alert | Honest tokio behavior + observable cap |

## 8 components — full module list

| # | Module | Path |
|---|---|---|
| 1 | `MoverRow` (26-col Copy struct) | `crates/common/src/mover_types.rs` |
| 2 | `MoversTracker` (extend with papaya + arena) | `crates/core/src/pipeline/top_movers.rs` |
| 3 | `Movers22TfWriter` × 22 instances | `crates/storage/src/movers_22tf_persistence.rs` |
| 4 | `MoversSnapshotScheduler` | `crates/core/src/pipeline/movers_22tf_scheduler.rs` |
| 5 | `Movers22TfWriterState` | `crates/core/src/pipeline/movers_22tf_writer_state.rs` |
| 6 | `MoversSupervisor` | `crates/core/src/pipeline/movers_22tf_supervisor.rs` |
| 7 | 9 new candle materialized views | `crates/storage/src/materialized_views.rs` |
| 8 | **Stock F&O expiry rollover constant flip** | `crates/common/src/constants.rs` + `crates/core/src/instrument/subscription_planner.rs` |

## DDL — 22 movers tables (final 26-column schema)

```sql
CREATE TABLE IF NOT EXISTS movers_{T} (
    ts                      TIMESTAMP,
    security_id             INT,
    segment                 SYMBOL CAPACITY 16 NOCACHE,        -- IDX_I / NSE_EQ / NSE_FNO
    instrument_type         SYMBOL CAPACITY 32 NOCACHE,        -- INDEX/EQUITY/OPTIDX/OPTSTK/FUTIDX/FUTSTK
    underlying_symbol       SYMBOL CAPACITY 1024 CACHE,
    underlying_security_id  INT,                               -- NSE_FNO → spot lookup
    expiry_date             DATE,                              -- NSE_FNO → month dropdown
    strike_price            DOUBLE,                            -- options display
    option_type             SYMBOL CAPACITY 4 NOCACHE,         -- CE / PE / null
    ltp                     DOUBLE,
    prev_close              DOUBLE,
    change_pct              DOUBLE,
    change_abs              DOUBLE,
    volume                  LONG,
    buy_qty                 LONG,
    sell_qty                LONG,
    open_interest           LONG,                              -- NSE_FNO only
    oi_change               LONG,                              -- NSE_FNO only
    oi_change_pct           DOUBLE,                            -- NSE_FNO only
    spot_price              DOUBLE,                            -- NSE_FNO underlying spot
    best_bid                DOUBLE,                            -- depth-aware
    best_ask                DOUBLE,                            -- depth-aware
    spread_pct              DOUBLE,                            -- (ask-bid)/ltp*100
    bid_pressure_5          LONG,                              -- sum top-5 bid qty
    ask_pressure_5          LONG,                              -- sum top-5 ask qty
    received_at             TIMESTAMP
) timestamp(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, security_id, segment);
```

## Hot-path latency — O(1) preserved

Per-tick (unchanged): papaya.get → SecurityState update → arithmetic = ~100 ns total.
Per-snapshot row: +130 ns spot lookup (NSE_FNO) + 30 ns depth lookup (NSE_EQ + NIFTY/BANKNIFTY ATM options).
Snapshot of 24K rows: ~10.5 ms (within 11 ms Criterion budget).

## Depth integration (Option A + C)

| Strategy | What |
|---|---|
| **Option A** (columns) | 5 depth columns populated at snapshot time from existing `MarketDepthCache` (papaya `HashMap<(u32, ExchangeSegment), DepthSnapshot>`) — already maintained by main feed Full packets and depth-20 connections |
| **Option C** (panels) | 6 Grafana panels query existing `market_depth` + `deep_market_depth` tables — NO new persistence |
| **Option B rejected** | Full orderbook ladder snapshots at 22 cadences would be ~4B rows/day, ~500GB/90d — too heavy |
| Existing depth-20/200 WS system | UNCHANGED — feeds the cache, that's it |

## Stock F&O expiry rollover (NEW, separate commit)

| Aspect | Old (current) | New (v3) |
|---|---|---|
| Constant | `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS = 1` | `= 0` |
| Wed with Thu expiry (T-1) | ROLL to next | **KEEP Thursday** |
| Thu IS expiry (T-0) | ROLL | ROLL (unchanged) |
| Tue with Thu expiry (T-2) | KEEP | KEEP (unchanged) |
| Indices | KEEP nearest (unchanged) | KEEP nearest (unchanged) |
| Reason | Old: avoid expiry-day risk | **Dhan disallows expiry-day stock F&O trading** |
| Source files | `subscription_planner::select_stock_expiry_with_rollover` | same |
| Rule file | `depth-subscription.md` 2026-04-24 Updates §6 | UPDATED |
| Runbook | `docs/runbooks/expiry-day.md` | UPDATED |

## Cross-references

- Risks: `.claude/plans/v2-risks.md`
- Ratchets: `.claude/plans/v2-ratchets.md`
- Phasing: `.claude/plans/v2-phases.md`
- Index: `.claude/plans/active-plan-movers-22tf-redesign-v2.md`
