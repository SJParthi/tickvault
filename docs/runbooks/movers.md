# Market Movers Runbook — 6-Bucket Taxonomy

> **Shipped:** 2026-04-22, PR #324 (Phase A + B + C1/C3 + E1 + F).
> **Plan:** `.claude/plans/archive/2026-04-22-6-bucket-movers.md`

## What This Is

A single movers engine (`MoversTrackerV2` in `crates/core/src/pipeline/top_movers.rs`)
that classifies every live tick into one of **6 independently-ranked buckets**:

| Bucket | Segment | Example |
|---|---|---|
| `indices` | IDX_I | NIFTY 50, BANKNIFTY, INDIA VIX |
| `stocks` | NSE_EQ / BSE_EQ | RELIANCE, TCS, INFY |
| `index_futures` | NSE_FNO / FUTIDX | NIFTY FUT Apr26 |
| `stock_futures` | NSE_FNO / FUTSTK | RELIANCE FUT Apr26 |
| `index_options` | NSE_FNO / OPTIDX | NIFTY 25000 CE |
| `stock_options` | NSE_FNO / OPTSTK | RELIANCE 3000 CE |

Each bucket maintains its own top-20 leaderboards for 3-7 categories
(`gainers`, `losers`, `most_active`, and on derivatives also `top_oi`,
`oi_buildup`, `oi_unwind`, `top_value`).

## Front-End URL Mapping

| UI label | URL (when `/api/movers` ships — Phase D) |
|---|---|
| Top OI (Index Options) | `GET /api/movers?bucket=index_options&category=top_oi` |
| Top OI (Stock Options) | `GET /api/movers?bucket=stock_options&category=top_oi` |
| Top Volume (Stocks) | `GET /api/movers?bucket=stocks&category=most_active` |
| Highest OI (Index Futures) | `GET /api/movers?bucket=index_futures&category=top_oi` |
| Top Gainers (Indices) | `GET /api/movers?bucket=indices&category=gainers` |
| Full snapshot | `GET /api/movers` |

## Prev-Close Source (Ticket #5525125)

- **Indices (IDX_I):** standalone code-6 PrevClose packet — one-shot push
  on subscribe. Populated via `MoversTrackerV2::update_prev_close`.
- **Stocks + Derivatives:** `close` field in Quote (bytes 38-41) / Full
  (bytes 50-53) packet. Read automatically from `ParsedTick::day_close`
  on every tick.

## Option Noise Filter

Default config (tunable in `config/base.toml [movers]`):

```toml
index_options_min_oi = 100
stock_options_min_oi = 100
```

Suppresses deep-OTM 0.05 → 0.10 = +100% garbage that would otherwise
dominate `price_gainers`. Set to `0` to disable.

## Observability

### Prometheus metrics (live today)

- `tv_movers_snapshot_duration_ms` — histogram, compute latency per
  recompute cycle (5s default).
- `tv_movers_tracked_total{bucket}` — gauge, per-bucket live instrument
  count. Six time series (one per bucket).

### Grafana dashboard

`deploy/docker/grafana/dashboards/market-movers.json` — one panel per
bucket + snapshot latency timeseries. UID: `tv-market-movers`.

### Alerts

- `MoversSnapshotMissing` (HIGH) — no snapshot emission for 5 min during
  market hours = tracker crashed or tick feed lost.
- `MoversTrackedCountDropped` (WARN) — `tv_movers_tracked_total{bucket}`
  fell > 50% in 5 min = WebSocket loss or registry rebuild.

## Schema (`top_movers` QuestDB table)

```sql
CREATE TABLE top_movers (
    bucket SYMBOL,              -- 'indices' | 'stocks' | ...
    rank_category SYMBOL,       -- 'gainers' | 'losers' | 'top_oi' | ...
    rank INT,                   -- 1..20
    security_id LONG,
    symbol SYMBOL,
    underlying SYMBOL,
    expiry STRING,
    strike DOUBLE,
    option_type SYMBOL,
    ltp DOUBLE,
    prev_close DOUBLE,
    change_pct DOUBLE,
    volume LONG,
    value DOUBLE,
    oi LONG,
    prev_oi LONG,
    oi_change_pct DOUBLE,
    ts TIMESTAMP
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, bucket, rank_category, rank);
```

## Known Limitations (to be addressed in follow-up)

1. **`oi_buildup` + `oi_unwind` always empty.** Dhan does NOT push
   previous-day OI on the NSE_FNO live feed. The fix is to snapshot
   OI at 09:15:00 IST and use that as the intraday baseline
   — documented in the plan but deferred to a follow-up PR.
2. **QuestDB ILP writer (Phase C2) not yet wired.** The table exists
   (Phase C1) and the snapshot struct serializes cleanly, but rows
   only land once the 1-Hz persistence task spawns in
   `trading_pipeline.rs` (Phase G1).
3. **`GET /api/movers` handler (Phase D1) not yet implemented.** The
   `/api/top-movers` and `/api/option-movers` endpoints continue to
   work with the legacy 2-bucket / mixed-NSE_FNO shapes until Phase D
   ships the unified endpoint.
4. **`MoversTrackerV2` not yet wired into `trading_pipeline.rs`.**
   The type compiles, the tests pass, but no tick stream feeds it in
   production yet. Phase G1.

## Regression Guards

Ratchet tests that prevent the above shipped work from regressing:

| Test | Guards |
|---|---|
| `pipeline::mover_classifier::tests::test_classify_instrument_*` | 12 classifier tests (A1) |
| `pipeline::mover_classifier::tests::test_mover_category_*` | 9 category tests (A2) |
| `pipeline::top_movers::tests::test_v2_*` | 11 tracker + snapshot tests (B1+B2) |
| `crates/storage/src/movers_persistence.rs::test_top_movers_*` | 7 DDL ratchets (C1+C3) |
| `crates/common/src/config.rs::test_movers_config_*` | 5 default-value tests (E1) |
| `session_2026_04_22_regression_guard::guard_movers_v2_*` | 2 metric-presence source scans (F1) |
| `grafana_dashboard_snapshot_filter_guard::critical_metric_prefixes_*` | `tv_movers_` prefix must have a panel (F2) |

## Related

- Plan file: `.claude/plans/archive/2026-04-22-6-bucket-movers.md`
- Classifier module: `crates/core/src/pipeline/mover_classifier.rs`
- Tracker + snapshot types: `crates/core/src/pipeline/top_movers.rs`
  (grep for `MoversTrackerV2` / `MoversSnapshotV2`)
- Config: `crates/common/src/config.rs::MoversConfig`
- Live-market-feed prev-close rules: `.claude/rules/dhan/live-market-feed.md`
