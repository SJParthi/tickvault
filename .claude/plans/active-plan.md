# Implementation Plan: Movers 15-Timeframe Expansion (1m–15m)

**Status:** APPROVED
**Date:** 2026-04-25
**Approved by:** Parthiban ("go ahead with recommended approach of d2 ... can you implement the remaining bro okay?")
**Branch:** `claude/improve-test-coverage-hk9Qx`

## Context

Today the V2 movers tracker (`crates/core/src/pipeline/top_movers.rs`)
emits a SINGLE snapshot covering 6 buckets (Indices, Stocks, IndexFutures,
StockFutures, IndexOptions, StockOptions) at a single 5-second cadence
with `change_pct` computed from `prev_close` (yesterday's close, IST 09:00
boundary). It does NOT yet support multi-timeframe leaderboards (e.g.
"top movers over the last 5 minutes").

User wants the same multi-timeframe pattern as `candles_*` materialized
views — 15 distinct timeframes (1m, 2m, 3m, ..., 15m), each computing
movers over its own rolling window, all 6 buckets visible at every
timeframe.

## Out of scope (explicit)

- Cross-verify against Dhan UI (REST forbidden, screenshot-only is too
  fragile for automation; user said "as of now skip this").
- Sub-minute timeframes (<1m too noisy) and post-15m timeframes (>15m
  → just use candle data directly).
- Any historical / REST API call into the movers pipeline (live-feed-purity
  rule).

## In scope

- 15 timeframes: 1m, 2m, 3m, 4m, 5m, 6m, 7m, 8m, 9m, 10m, 11m, 12m, 13m, 14m, 15m
- 6 buckets × up-to-9 leaderboards = up to 54 ranked lists per timeframe
- Leaderboards per bucket (matches Dhan UI verbatim):

| Bucket | Leaderboards | Count |
|--------|--------------|-------|
| Indices | Price Gainers, Price Losers | 2 |
| Stocks | Price Gainers, Price Losers | 2 |
| IndexFutures | Premium, Discount, Top Volume, OI Gainers, OI Losers, Price Gainers, Price Losers | 7 |
| StockFutures | Premium, Discount, Top Volume, OI Gainers, OI Losers, Price Gainers, Price Losers | 7 |
| IndexOptions | Highest OI, OI Gainers, OI Losers, Top Volume, Top Value, Price Gainers, Price Losers | 7 |
| StockOptions | Highest OI, OI Gainers, OI Losers, Top Volume, Top Value, Price Gainers, Price Losers | 7 |

- Storage: single `top_movers` QuestDB table extended with `timeframe`
  SYMBOL column. DEDUP key extended to include `timeframe` and `segment`
  (per I-P1-11). PARTITION BY DAY unchanged.
- Snapshot writer per timeframe: each timeframe boundary triggers a
  ranking pass and a write to QuestDB.
- Tests: schema, DEDUP, Timeframe enum, premium/discount field presence,
  rolling baseline correctness, single-timeframe ranking determinism.

## Plan items

- [ ] **A. `Timeframe` enum + 15 variants** in `crates/core/src/pipeline/top_movers.rs`.
  - `pub enum Timeframe { OneMin, TwoMin, …, FifteenMin }`
  - `as_str()` returns `"1m"`, `"2m"`, …, `"15m"` (DB symbol)
  - `secs()` returns 60, 120, …, 900
  - `iter_all()` returns `&'static [Timeframe; 15]` so callers don't allocate
  - Tests: `test_timeframe_as_str_stable`, `test_timeframe_secs_match_minutes`,
    `test_timeframe_count_is_15`, `test_timeframe_iter_all_in_ascending_order`

- [ ] **B. Add `Premium` + `Discount` `Vec<MoverEntryV2>` to `DerivativeBucket`**
  in `top_movers.rs`. Default empty. Population requires spot-of-underlying
  lookup — wired in step F.
  - Tests: `test_derivative_bucket_default_premium_discount_empty`,
    `test_derivative_bucket_premium_separate_from_discount`

- [ ] **C. Schema: add `timeframe` SYMBOL to `top_movers` table + fix DEDUP key**
  in `crates/storage/src/movers_persistence.rs`.
  - DDL: insert `timeframe SYMBOL,` between `bucket` and `rank_category`
  - DEDUP key: `"timeframe, bucket, rank_category, rank, security_id, segment"`
    (was `"bucket, rank_category, rank"` — added `timeframe`, `security_id`, `segment`)
  - Add `segment SYMBOL` column too (was missing — I-P1-11 violation in
    current schema; segment is needed for the cross-segment composite key).
  - `ALTER TABLE ADD COLUMN IF NOT EXISTS` for both new columns at boot
    (schema self-heal pattern from observability-architecture.md).
  - Tests: `test_top_movers_ddl_contains_timeframe_column`,
    `test_top_movers_ddl_contains_segment_column`,
    `test_top_movers_dedup_key_contains_timeframe_and_segment_and_security_id`,
    `test_top_movers_alter_table_self_heal_emits_correct_ddl`

- [ ] **D. Extend `TopMoverRow` struct with `timeframe: &'static str` + `segment: &'static str`**
  in `movers_persistence.rs`. Update `entry_to_row`, `flatten_*_bucket`
  signatures to accept `Timeframe` + segment-resolver. Update ILP write
  path to emit both columns.
  - Tests: `test_top_mover_row_serializes_timeframe`,
    `test_top_mover_row_serializes_segment`,
    `test_flatten_emits_timeframe_for_every_row`

- [ ] **E. Per-timeframe rolling baseline state**
  - New struct `TimeframeBaselines` (in a new sibling module
    `crates/core/src/pipeline/movers_window.rs`):
    - `HashMap<(SecurityId, ExchangeSegment), CircularBuffer<(IstSecs, f32)>>` —
      bounded ring of (timestamp, price) per security covering up to 15 minutes
    - `with_capacity(security_count)` to size up-front (no rehash on hot path)
    - `record(security_id, segment, ist_secs, ltp)` — O(1) push; trims
      entries older than 15 min so the buffer stays bounded
    - `baseline_at(security_id, segment, lookback_secs)` — O(log buffer-len)
      binary search on circular buffer; returns the price at or just after
      `now - lookback_secs`. Returns `None` if no datapoint that old yet.
  - Tests: `test_baselines_record_then_lookup`,
    `test_baselines_trims_older_than_15_min`,
    `test_baselines_lookup_returns_none_when_warming_up`,
    `test_baselines_binary_search_returns_closest_to_target`,
    `test_baselines_segment_isolation` (same security_id, different segment
    → independent buffers per I-P1-11)

- [ ] **F. Per-timeframe ranking pass** in `MoversTrackerV2::snapshot()`:
  - For each `Timeframe` in `Timeframe::iter_all()`:
    - For each tracked security: compute `change_pct = (ltp − baseline(tf.secs())) / baseline × 100`
    - For derivatives, compute `premium = ltp − spot_of_underlying(security)` and
      route to Premium (>0 ranked desc) or Discount (<0 ranked asc) leaderboard
    - For options, populate `Highest OI` (sort by current `oi` desc) and
      `Top Value` (sort by `ltp × volume` desc)
    - For OI Gainers / OI Losers: leave empty (matches existing comment) —
      requires 09:15 IST OI baseline, separate follow-up PR
  - Output type: `Vec<(Timeframe, MoversSnapshotV2)>` returned from `snapshot_all()`
  - Tests: `test_snapshot_all_returns_15_entries`,
    `test_premium_only_for_futures_buckets`,
    `test_premium_zero_when_ltp_equals_spot`,
    `test_premium_negative_routed_to_discount`,
    `test_highest_oi_only_for_options_buckets`,
    `test_top_value_only_for_options_buckets`

- [ ] **G. Snapshot loop + writer wiring** in tracker entry point:
  - Cadence: every 60 seconds, run `snapshot_all()` then write each
    timeframe to `top_movers` with the matching `timeframe` symbol
  - Market-hours gate: skip outside 09:00–15:30 IST (matches
    `is_within_market_hours_ist()` from depth_rebalancer.rs)
  - Each snapshot row gets `ts = ist_now_secs() × 1_000_000_000`
    (no IST offset — already IST per data-integrity.md WebSocket rule)
  - Tests (source-scan ratchets, since the loop is async):
    `test_movers_snapshot_loop_calls_snapshot_all`,
    `test_movers_snapshot_loop_market_hours_gated`,
    `test_movers_snapshot_loop_writes_to_top_movers_table`

- [ ] **H. Spot-price lookup for premium/discount**:
  - Reuse `SharedSpotPrices: Arc<RwLock<HashMap<u32, f32>>>` from
    `depth_rebalancer.rs` (already populated by tick broadcast subscriber)
  - For each derivative, `underlying_security_id` from `InstrumentRegistry`
    → spot_price lookup → premium computation
  - O(1) hot path read (RwLock read + HashMap lookup)
  - Tests: `test_premium_lookup_with_known_spot_price`,
    `test_premium_lookup_returns_none_when_spot_missing` (warmup grace),
    `test_premium_uses_underlying_not_derivative_security_id`

- [ ] **I. DDL self-heal at boot** in `ensure_movers_tables`:
  - `ALTER TABLE top_movers ADD COLUMN IF NOT EXISTS timeframe SYMBOL`
  - `ALTER TABLE top_movers ADD COLUMN IF NOT EXISTS segment SYMBOL`
  - Migration-safe for existing deployments without DROP
  - Tests: `test_ensure_movers_tables_emits_alter_for_timeframe`,
    `test_ensure_movers_tables_emits_alter_for_segment`

- [ ] **J. Banned-pattern protection (live-feed-purity extension)**:
  - Add to `.claude/hooks/banned-pattern-scanner.sh` category 6: any new
    file under `crates/core/src/pipeline/movers_*.rs` that imports
    `historical::` modules → block. (Movers pipeline is live-only.)
  - Test: `test_banned_pattern_scanner_blocks_historical_in_movers`

## Files modified

| File | Reason |
|------|--------|
| `crates/core/src/pipeline/top_movers.rs` | Timeframe enum, premium/discount, snapshot_all() |
| `crates/core/src/pipeline/movers_window.rs` (NEW) | TimeframeBaselines rolling state |
| `crates/core/src/pipeline/mod.rs` | `pub mod movers_window;` |
| `crates/storage/src/movers_persistence.rs` | Schema + DEDUP + ALTER, ILP writes |
| `crates/common/src/constants.rs` | `MOVERS_SNAPSHOT_INTERVAL_SECS = 60`, `MOVERS_TIMEFRAME_COUNT = 15` |
| `.claude/hooks/banned-pattern-scanner.sh` | Category 6 movers extension |
| `crates/storage/tests/dedup_segment_meta_guard.rs` | Should already cover the new DEDUP — verify |

## Tests added (35 total)

A: 4. B: 2. C: 4. D: 3. E: 5. F: 6. G: 3. H: 3. I: 2. J: 1. Total: 33 + ratchets covered by existing meta-guards.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot fresh QuestDB, run movers loop for 16 minutes | First 15 min: progressive backfill of timeframe rows (1m at minute 1, 2m at minute 2, ..., 15m at minute 15). After minute 15: all 15 timeframes write every minute |
| 2 | Same security_id in NSE_FNO + BSE_FNO | Both rows persisted (segment in DEDUP key); independent baselines |
| 3 | Spot price missing for an underlying | Premium/Discount empty for that derivative; other leaderboards still populated |
| 4 | Outside 09:00–15:30 IST | Snapshot loop skips; no writes to QuestDB |
| 5 | Restart mid-day | Baselines reset to empty; first 15 min of post-restart snapshots have `None` change_pct → those leaderboards empty until warmup completes; existing rows in QuestDB are untouched |

## Future work (separate PR)

- OI baseline at 09:15 IST snapshot → enables OI Gainers/Losers
- Grafana dashboard panels per timeframe
- API handler `/api/movers?timeframe=5m&bucket=index_futures&leaderboard=premium`
- Cross-verify (REST or screenshot) once user re-prioritises
