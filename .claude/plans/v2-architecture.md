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

## Section I — Depth-20 dynamic top-150 + Depth-200 URL wipe-off (NEW 2026-04-28)

### Depth-20 dynamic top-150 (Phase 7)

| Aspect | Detail |
|---|---|
| Connection layout | 5 total depth-20 connections; slots 1+2 stay pinned to NIFTY ATM±24 + BANKNIFTY ATM±24 (~98 instruments); slots 3/4/5 dynamic (3 × 50 = 150 contracts) |
| Selection criteria (Option B confirmed) | `SELECT security_id FROM option_movers WHERE ts > now() - 90s AND change_pct > 0 ORDER BY volume DESC LIMIT 150` |
| Recompute interval | 1 minute via `tokio::time::interval` + `MissedTickBehavior::Skip` |
| Market-hours gate | `is_within_market_hours_ist()` short-circuits outside [09:00, 15:30] IST |
| Delta computation | `HashSet<(u32, ExchangeSegment)>` diff: `leavers = previous - current`; `entrants = current - previous` |
| Wire mechanism | Reuses existing `DepthCommand::Swap20 { unsubscribe_messages, subscribe_messages }` channel — zero-disconnect |
| ErrorCodes | `DEPTH-DYN-01` (top-150 empty: option_movers stale or empty universe) Severity::High; `DEPTH-DYN-02` (Swap20 channel broken) Severity::Critical — promoted from RESERVED to defined in Phase 7 |
| Module | `crates/core/src/instrument/depth_20_dynamic_subscriber.rs` (new) |
| Boot wiring | `crates/app/src/main.rs` after existing depth-20 setup; spawns one supervisor task that owns the 3 dynamic-slot mpsc::Sender channels |
| Source dependency | EXISTING `option_movers` table (already populated 60s by `OptionMoversWriter`) — does NOT depend on new `movers_{T}` tables |
| Failure mode | If selector returns < 150 (e.g., low-volume morning), still emit Swap20 with whatever was returned + log; do not panic |
| Backpressure | If `Swap20` channel send fails, log + retry next minute; supervisor respawns connection task on panic (existing WS-GAP-05) |

### Depth-200 URL wipe-off (Phase 6)

| Item | Action |
|---|---|
| `crates/core/examples/depth_200_variants.rs` (~400 LoC, 8-variant A/B test) | DELETE — `/` is locked-fact since 2026-04-23 |
| `crates/core/src/websocket/depth_connection.rs` historical narrative comments | STRIP comments at lines ~860, ~1016, ~1085, ~1571, ~1767 referencing the old `/twohundreddepth` story; keep the assertion test + one-line LOCKED FACT marker |
| `crates/common/tests/dhan_locked_facts.rs` ratchet | KEEP unchanged — this is the regression-blocker |
| `crates/common/src/config.rs:154` | KEEP — one-line ratchet pointer |
| Banned-pattern hook category 4 | KEEP — blocks any literal `wss://full-depth-api.dhan.co/twohundreddepth` regression |

**SELF token confirmation (Parthiban 2026-04-28):** Depth-200 accepts ONLY SELF tokens. RenewToken extends a SELF token by 24h while preserving `tokenConsumerType: "SELF"`. APP tokens are silently rejected by the depth-200 server (TCP disconnects within seconds). This is enforced by `Depth200SelfTokenManager::boot_from_ssm`'s `validate_self_claims` (DEPTH200-AUTH-01..03 fire on misuse).

## Cross-references

- Risks: `.claude/plans/v2-risks.md`
- Ratchets: `.claude/plans/v2-ratchets.md`
- Phasing: `.claude/plans/v2-phases.md`
- Index: `.claude/plans/active-plan-movers-22tf-redesign-v2.md`
