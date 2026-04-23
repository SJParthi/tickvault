# Implementation Plan: 6-Bucket Movers Taxonomy (Indices / Stocks / Index-Fut / Stock-Fut / Index-Opt / Stock-Opt)

**Status:** PARTIAL_VERIFIED (10 of 17 plan items shipped — foundation complete, production wiring deferred)
**Date:** 2026-04-22
**Approved by:** Parthiban (2026-04-22)
**Verified by:** session closeout 2026-04-22
**Branch:** `claude/market-feed-depth-explanation-RynUx`

## Session Outcome Summary (2026-04-22)

**Shipped (10 phases, 9 commits on PR #324):**

| Phase | Item | Commit | Tests |
|---|---|---|---|
| Plan | Status flip DRAFT→APPROVED | `bc762eb` | — |
| A1 | `MoverBucket` enum + `classify_instrument` + 6 bucket dispatch | `15097e1` | 12 |
| A2 | `MoverCategory` enum + `is_applicable_to` OI gating | `15097e1` | 11 |
| B1+B2 | `MoversTrackerV2` + `MoversSnapshotV2` + `PriceBucket` + `DerivativeBucket` | `2eb8532` | 11 |
| B3 | `option_movers.rs` marked superseded (doc-only) | `f61c65a` | 0 |
| C1+C3 | `top_movers` QuestDB DDL + DEDUP + table-name constant | `5d496c0` | 7 |
| E1 | `[movers]` TOML section + `MoversConfig` in `AppConfig` | `d83957c` | 5 |
| F1 | `tv_movers_snapshot_duration_ms` histogram + `tv_movers_tracked_total{bucket}` gauges | `c3cd69e` | 2 ratchets |
| F2 | `market-movers.json` Grafana dashboard (6 bucket stats + 2 latency panels) | `4b2df28` | — |
| F3 | `MoversSnapshotMissing` + `MoversTrackedCountDropped` Prometheus alerts | `4b2df28` | — |
| Runbook | `docs/runbooks/movers.md` with UI-label → URL mapping | (this commit) | — |

**Test count added:** 48 new tests across `tickvault-core`, `tickvault-storage`,
`tickvault-common` — all passing locally in < 2s scoped.

**Ratchet coverage:** the 48 tests + dashboard `critical_metric_prefixes` scan
+ schema-column scan mean a regression (rename, delete, silently drop a bucket
or metric) breaks `cargo test` deterministically.

### Deferred to a follow-up session (7 items — all require live-env integration testing)

- **C2 — Unified ILP writer.** Table exists, DDL locked, DEDUP wired. Writer
  needs to serialize snapshot rows and flush to QuestDB ILP on a 1-Hz tokio
  task with market-hours gating. Risk: needs a running QuestDB instance to
  smoke-test the ILP batch flush. Estimated +~300 LoC + 6 integration tests.
- **D1 — `GET /api/movers` HTTP handler.** JSON shape and query-param
  validation are documented in the plan + runbook. Handler needs an
  `axum` router wire-up and `SharedMoversSnapshotV2` state plumbing.
  Estimated +~150 LoC + 8 tests.
- **D2 — back-compat shims.** Keep `/api/top-movers` and `/api/option-movers`
  returning their legacy shapes for one release cycle to avoid breaking
  existing consumers. Estimated +~80 LoC.
- **G1 — wire `MoversTrackerV2` into `trading_pipeline.rs`.** Inject
  `Arc<InstrumentRegistry>` at boot, spawn the 1-Hz recompute task,
  funnel every tick through `update_v2()`. Risk: adds a second tick
  consumer alongside the legacy `TopMoversTracker` + `OptionMoversTracker`;
  transition window needs careful sequencing to avoid double accounting.
  Estimated +~100 LoC + deploy smoke test.
- **G2 — `MoversConfig` wiring in `trading_pipeline.rs`.** Read the
  `[movers]` section from `AppConfig` and pass to `MoversTrackerV2::new`
  for filter thresholds. Needs G1 first. Estimated +~30 LoC.
- **OI buildup/unwind baseline.** The `oi_buildup` + `oi_unwind` lists
  are empty by design today (documented in the tracker) — Dhan does NOT
  push prev-day OI on the NSE_FNO live feed. Fix is to snapshot OI at
  09:15:00 IST and use that as the intraday baseline. Would populate
  those two lists meaningfully.
- **H3 — manual smoke test.** Can't run without G1 wiring live.

These items are **production wiring**, not foundation. Every shipped piece
of code is self-contained and unit-tested. The unshipped work is pure
integration: connecting the tracker to the live tick stream, the QuestDB
writer, and the HTTP router. All three can be done in a single follow-up
session without any new design decisions.

## Goal

Parthiban directive (2026-04-22): *"our idea is to always separate indices stocks futures even in futures also indices futures separately and stocks futures separately and for options also stocks options separately and indices options separately"*.

Today the movers infra has:

- `crates/core/src/pipeline/top_movers.rs` (1,300 LoC) — **stocks + indices** only. 6 lists on the snapshot: `equity_{gainers,losers,most_active}` + `index_{gainers,losers,most_active}`.
- `crates/core/src/pipeline/option_movers.rs` (799 LoC) — **all NSE_FNO mixed together** in one bucket, 7 ranking categories.
- `crates/storage/src/movers_persistence.rs` (1,539 LoC) — QuestDB ILP writer.
- `crates/api/src/handlers/top_movers.rs` (623 LoC) — HTTP handler.

The front-end needs **6 separate, independently ranked buckets**, not 2. Futures and options must be split by whether the underlying is an index or a stock.

## The 6 Buckets (canonical taxonomy)

Classified by `(exchange_segment, category, instrument_kind)` — resolved via `InstrumentRegistry::get_with_segment` (I-P1-11 compliant).

| # | Bucket | Segment | Category | Instrument Kind | Example |
|---|---|---|---|---|---|
| 1 | `indices` | IDX_I | Index | — | NIFTY 50, BANKNIFTY, INDIA VIX |
| 2 | `stocks` | NSE_EQ / BSE_EQ | Equity | — | RELIANCE, TCS, INFY |
| 3 | `index_futures` | NSE_FNO | Derivative | FutureIndex (FUTIDX) | NIFTY FUT Apr26 |
| 4 | `stock_futures` | NSE_FNO | Derivative | FutureStock (FUTSTK) | RELIANCE FUT Apr26 |
| 5 | `index_options` | NSE_FNO | Derivative | OptionIndex (OPTIDX) | NIFTY 28-APR 25000 CE |
| 6 | `stock_options` | NSE_FNO | Derivative | OptionStock (OPTSTK) | RELIANCE 28-APR 3000 CE |

## Ranking Categories per Bucket

| Ranking | Indices | Stocks | Index Fut | Stock Fut | Index Opt | Stock Opt |
|---|:-:|:-:|:-:|:-:|:-:|:-:|
| `gainers` (%) | Y | Y | Y | Y | Y | Y |
| `losers` (%) | Y | Y | Y | Y | Y | Y |
| `most_active` (volume) | Y | Y | Y | Y | Y | Y |
| `top_oi` (absolute OI) | — | — | Y | Y | Y | Y |
| `oi_buildup` (ΔOI %) | — | — | Y | Y | Y | Y |
| `oi_unwind` (ΔOI % negative) | — | — | Y | Y | Y | Y |
| `top_value` (LTP × volume) | — | — | Y | Y | Y | Y |

Top-N = 20 per category per bucket. Snapshot size bound: `(2 × 3 × 20) + (4 × 7 × 20) = 680 entries`.

## Front-End Query Mapping (operator-facing)

Parthiban: *"even we have our front end implementation which is top oi top volume highest oi"*.

| UI label | Bucket + category | URL |
|---|---|---|
| Top OI (Index Options) | `index_options` / `top_oi` | `GET /api/movers?bucket=index_options&category=top_oi` |
| Top OI (Stock Options) | `stock_options` / `top_oi` | `GET /api/movers?bucket=stock_options&category=top_oi` |
| Top Volume (Stocks) | `stocks` / `most_active` | `GET /api/movers?bucket=stocks&category=most_active` |
| Highest OI (Index Futures) | `index_futures` / `top_oi` | `GET /api/movers?bucket=index_futures&category=top_oi` |
| Top Gainers (Indices) | `indices` / `gainers` | `GET /api/movers?bucket=indices&category=gainers` |
| All categories, all buckets | — | `GET /api/movers` (full snapshot) |

## Plan Items

### Phase A — Core taxonomy + classifier (standalone, no production wiring)

- [ ] **A1.** Add `MoverBucket` enum + `as_str()` + `from_instrument_registry` classifier
  - File: `crates/core/src/pipeline/mover_classifier.rs` (new, ~120 LoC)
  - 6 variants: `Indices`, `Stocks`, `IndexFutures`, `StockFutures`, `IndexOptions`, `StockOptions`
  - Stable wire strings: `"indices"`, `"stocks"`, `"index_futures"`, `"stock_futures"`, `"index_options"`, `"stock_options"`
  - Classifier: `fn classify(registry: &InstrumentRegistry, security_id: u32, segment: ExchangeSegment) -> Option<MoverBucket>`
  - Tests (12): 2 per bucket (happy + edge) + 2 for unknown instrument + 2 for Display/Clone round-trip

- [ ] **A2.** Add `MoverCategory` enum + `as_str()`
  - File: `crates/core/src/pipeline/mover_classifier.rs` (same file)
  - 7 variants: `Gainers`, `Losers`, `MostActive`, `TopOi`, `OiBuildup`, `OiUnwind`, `TopValue`
  - Stable wire strings matching the UI label column
  - `fn is_applicable_to(bucket: MoverBucket) -> bool` — e.g. `TopOi` only valid for derivatives (buckets 3-6)
  - Tests (9): applicability matrix per bucket

### Phase B — Unified tracker (replaces the two existing trackers)

- [ ] **B1.** New `MoversTrackerV2` with 6 buckets in one struct
  - File: `crates/core/src/pipeline/top_movers.rs` (extend existing, +~400 LoC)
  - Single `HashMap<(SecurityId, u8), SecurityState>` keyed O(1) per tick (unchanged).
  - `SecurityState` gains `bucket: MoverBucket` (1-byte tag) resolved once at first tick via injected `Arc<InstrumentRegistry>`.
  - `update()` stays O(1): HashMap lookup + bucket tag already cached + arithmetic.
  - `compute_snapshot()` — single pass over the HashMap, dispatches each entry into 6 per-bucket BinaryHeaps sized K=20. O(N log K) total. ~680 entries out.
  - Zero-alloc hot path — ArrayVec where possible, pre-allocated heap capacity.

- [ ] **B2.** New `MoversSnapshotV2` struct
  - File: `crates/core/src/pipeline/top_movers.rs`
  - Shape:
    ```rust
    pub struct MoversSnapshotV2 {
        pub indices: PriceBucket,             // gainers, losers, most_active
        pub stocks: PriceBucket,              // gainers, losers, most_active
        pub index_futures: DerivativeBucket,  // + top_oi, oi_buildup, oi_unwind, top_value
        pub stock_futures: DerivativeBucket,
        pub index_options: DerivativeBucket,
        pub stock_options: DerivativeBucket,
        pub total_tracked: usize,
        pub snapshot_at_ist_secs: i64,
    }
    ```
  - Tests (6): one per bucket — inject N ticks for that bucket + verify only that bucket populated.

- [ ] **B3.** Deprecate `option_movers.rs` module
  - Mark `pub struct OptionMoversTracker` with `#[deprecated(note = "replaced by MoversTrackerV2; see plan items A1/B1")]`.
  - Keep file for one release cycle so callers migrate.
  - Deletion ticket: follow-up PR next session.

### Phase C — Persistence (QuestDB)

- [ ] **C1.** New `top_movers` QuestDB table (unified schema)
  - DDL location: `crates/storage/src/movers_persistence.rs` (extend).
  - Schema:
    ```sql
    CREATE TABLE IF NOT EXISTS top_movers (
      bucket SYMBOL,              -- 'indices' | 'stocks' | ... (plan A1 wire strings)
      rank_category SYMBOL,       -- 'gainers' | 'losers' | ... (plan A2 wire strings)
      rank INT,                   -- 1..20
      security_id LONG,
      symbol SYMBOL,
      underlying SYMBOL,          -- nullable for stocks/indices
      expiry STRING,              -- nullable for stocks/indices
      strike DOUBLE,              -- nullable for non-options
      option_type SYMBOL,         -- 'CE' | 'PE' | null
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
  - Partition by day = SEBI 5-year retention via lifecycle tiers.
  - f32→f64 via `f32_to_f64_clean()` per STORAGE-GAP-02.
  - **Timestamp rule:** `ts` = snapshot-emit IST epoch seconds × 1_000_000_000. NOT UTC, NO +5:30 (it's a snapshot-emit time, same rule as tick ts).

- [ ] **C2.** Unified ILP writer
  - Extend `movers_persistence.rs` with `write_v2_snapshot(snapshot: &MoversSnapshotV2)`.
  - Cadence: 1 Hz during market hours (09:15-15:30 IST). Gated by `is_within_market_hours_ist()` helper.
  - Writes at most 680 rows/second. At 6 hrs × 3600 s × 680 = ~14.7M rows/day. QuestDB partition+DEDUP handles this.

- [ ] **C3.** Ratchet tests
  - `test_top_movers_ddl_contains_required_columns`
  - `test_top_movers_ddl_partitioned_by_day`
  - `test_top_movers_ddl_wal_enabled`
  - `test_top_movers_ddl_dedup_keys_include_bucket_category_rank`
  - `test_top_movers_table_name_constant_matches_ddl`

### Phase D — HTTP API

- [ ] **D1.** Extend `/api/movers` handler
  - File: `crates/api/src/handlers/top_movers.rs` (extend; keep existing `/api/top-movers` for back-compat).
  - New route: `GET /api/movers`
  - Query params:
    - `bucket` = indices | stocks | index_futures | stock_futures | index_options | stock_options (optional; default = all)
    - `category` = gainers | losers | most_active | top_oi | oi_buildup | oi_unwind | top_value (optional; default = all)
    - `top` = integer 1..20 (default 20)
  - Response:
    ```json
    {
      "snapshot_at_ist": "2026-04-22T14:30:00+05:30",
      "buckets": {
        "indices": { "gainers": [...], "losers": [...], "most_active": [...] },
        "stocks": { ... },
        "index_futures": { ... },
        "stock_futures": { ... },
        "index_options": { ... },
        "stock_options": { ... }
      }
    }
    ```
  - Invalid `bucket`/`category` → 400 with error message listing valid values.
  - 405 if `category=top_oi` requested for `bucket=indices` or `stocks` (not applicable).

- [ ] **D2.** Back-compat shim
  - Keep `/api/top-movers` returning the old 2-bucket shape for one release.
  - Keep `/api/option-movers` returning the old 7-category mixed-derivative shape for one release.
  - Both mark `#[deprecated]` in handler docs.

### Phase E — Options-specific filter rules (noise suppression)

- [ ] **E1.** Min-volume / min-OI filter per bucket, config-driven
  - File: `config/base.toml` — new `[movers]` section.
  - Config (with rationale in comments):
    ```toml
    [movers]
    stocks_min_volume = 0             # no filter — every NSE_EQ is liquid enough
    indices_min_volume = 0
    index_futures_min_volume = 1      # require today's tick trade
    stock_futures_min_volume = 1
    index_options_min_volume = 1
    index_options_min_oi = 100        # suppress deep-OTM noise
    stock_options_min_volume = 1
    stock_options_min_oi = 100
    snapshot_cadence_secs = 5         # in-memory recompute period
    persistence_cadence_secs = 1      # QuestDB write period
    top_n = 20
    ```
  - Apply filter inside `compute_snapshot` before pushing to heap.
  - Tests (6): one per derivative bucket — tick with OI below min is filtered; tick with OI at min is kept.

### Phase F — Prometheus + Grafana

- [ ] **F1.** Prometheus metrics (3 new)
  - `tv_movers_snapshot_duration_ms` (histogram) — `compute_snapshot` latency per bucket.
  - `tv_movers_rows_written_total{bucket}` (counter) — QuestDB rows persisted per bucket.
  - `tv_movers_tracked_total{bucket}` (gauge) — live instrument count per bucket.

- [ ] **F2.** Grafana dashboard — new `market-movers.json`
  - 6 panels, one per bucket: stat panels for top-5 gainers + top-5 losers, plus OI panels for the 4 derivative buckets.
  - Data source: QuestDB via existing datasource proxy.
  - Link to this file from `market-data.json` dashboard.

- [ ] **F3.** Alert rules in `tickvault-alerts.yml`
  - `MoversSnapshotMissing` — `absent(tv_movers_rows_written_total) during 09:15-15:30 IST for 5m` → HIGH
  - `MoversRowCountDropped` — `sum(tv_movers_rows_written_total) per day drops > 50% vs prior day` → WARNING

### Phase G — Boot wiring + integration

- [ ] **G1.** Replace `TopMoversTracker` + `OptionMoversTracker` construction in `trading_pipeline.rs`
  - Inject `Arc<InstrumentRegistry>` at tracker construction (Phase B1).
  - Wire to tick-processor SPSC — one `update()` call per tick. Current code has two calls (stocks tracker + options tracker); collapse to one.
  - Spawn 1-Hz persistence task via `tokio::select! { _ = tokio::time::interval(1s) => { tracker.compute_snapshot(); persist(...); } }`.
  - Wall-clock + market-hours gate per Rule 3 (market-hours rule file).

- [ ] **G2.** Config wiring
  - `AppConfig` gets `movers: MoversConfig` field (type from Phase E1).
  - TOML load test verifying all fields default correctly.

### Phase H — Verification

- [ ] **H1.** Full scoped test suite: `cargo test -p tickvault-core`, `-p tickvault-storage`, `-p tickvault-api`.
- [ ] **H2.** `make scoped-check` clean.
- [ ] **H3.** One manual smoke: start local app, hit `/api/movers?bucket=indices`, verify NIFTY/BANKNIFTY appear with sensible change_pct.
- [ ] **H4.** Update the 6 existing runbooks + write a new `docs/runbooks/movers.md` with the UI-label → URL mapping from the Front-End Query Mapping table above.
- [ ] **H5.** `plan-verify.sh` → Status: VERIFIED → archive to `.claude/plans/archive/`.

## Scope Summary

| Phase | Files touched | Net LoC | New tests |
|---|---|---:|---:|
| A | 1 new (classifier) | +120 | 21 |
| B | top_movers.rs extended | +400 | 6 |
| C | movers_persistence.rs extended + DDL | +200 | 5 |
| D | handlers/top_movers.rs extended | +150 | 8 |
| E | config + compute_snapshot filter | +80 | 6 |
| F | new dashboard JSON + alerts YAML | +250 | 3 |
| G | trading_pipeline.rs + main.rs wiring | +100 | 4 |
| **Total** | **~9 files** | **~1,300 LoC** | **~53 tests** |

## Real Tradeoffs (call out before you approve)

1. **ONE table vs SIX tables?** I'll use ONE `top_movers` table with `bucket SYMBOL` column. Easier to query (`WHERE bucket=?`), smaller DDL surface, single write path. Alternative: six tables, per-bucket partitioning. Recommendation: one.

2. **Replace or keep `option_movers.rs`?** I'll deprecate it this PR and delete next PR. Keeps back-compat for one release.

3. **Snapshot cadence.** In-memory recompute every 5s (config-driven), QuestDB write every 1s (config-driven). Per-tick recompute is wasteful — humans can't read faster than 1 Hz; algo consumers read the in-memory snapshot directly, no QuestDB round-trip needed.

4. **Min-OI filter = 100 for options.** Suppresses deep-OTM 0.05→0.10 = +100% noise. Configurable. Zero if operator wants raw.

5. **Classification dependency.** Tracker needs `Arc<InstrumentRegistry>` at construct time. Registry is built at boot step 8 in the 15-step boot sequence; tracker spawns at step 11 (pipeline); dep chain is clean.

## Scenarios Covered

| # | Scenario | Expected behavior |
|---|---|---|
| 1 | NIFTY IDX_I tick with prev_close from code-6 packet | Appears in `indices.gainers/losers/most_active` |
| 2 | RELIANCE NSE_EQ tick with close from Full packet | Appears in `stocks.*` |
| 3 | NIFTY FUT NSE_FNO tick | Appears in `index_futures.*`, NOT `indices.*` |
| 4 | RELIANCE FUT NSE_FNO tick | Appears in `stock_futures.*` |
| 5 | NIFTY 25000 CE tick | Appears in `index_options.*` |
| 6 | RELIANCE 3000 CE tick | Appears in `stock_options.*` |
| 7 | Deep-OTM option with OI < 100 | Filtered out of all rankings |
| 8 | Tick with LTP > 0 but no prev_close yet | Skipped until prev_close arrives |
| 9 | I-P1-11 collision (id=27 IDX_I + NSE_EQ) | Classified correctly per segment; both appear in respective buckets |
| 10 | Request `/api/movers?bucket=indices&category=top_oi` | 405 Method Not Allowed (top_oi not applicable to indices) |
| 11 | Post-market snapshot | Persistence gated by market hours; no rows written |
| 12 | Restart mid-day | Tracker rebuilds from fresh ticks; no historical backfill (LIVE-FEED-PURITY compliance) |

## What This Plan Does NOT Include

- Historical mover data (aggregating past days into a leaderboard) — separate feature.
- Per-sector mover buckets (NIFTY_IT, NIFTY_BANK, etc.) — requires index-constituency lookup; can be added as a v2 bucket later.
- Greeks-based option rankings (highest IV, highest delta change) — Greeks pipeline is separate; can be added by extending `DerivativeBucket`.
- BSE_FNO derivatives — Dhan docs explicitly exclude BSE from depth + option chain; ignored per existing `option_movers.rs` comment.

## Archived / superseded plans

- `.claude/plans/archive/2026-04-22-unified-0912-subscription.md` — yesterday's Phase 2 / depth plan, PARTIAL_VERIFIED (23 of 25 items shipped).
