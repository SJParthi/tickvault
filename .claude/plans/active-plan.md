# Active Plan: 1d timeframe = historical-only (never tick-calculated)

**Status:** APPROVED (design locked) — implementation in progress
**Date:** 2026-06-02
**Approved by:** Parthiban — "1 day timeframe should never ever be calculated, always pulled once and only from historical 1 day timeframe" + chose **"Add OI to the daily pull"** when asked how prev-OI sources its OI.

## Verified facts (from 3-agent adversarial review + my own code verification)

1. **The write boundary** for `candles_1d` is TWO `on_seal` closures in `crates/app/src/main.rs` (~line 6003 per-tick + ~6123 boundary force-seal). The aggregator (`multi_tf_aggregator.rs`) still seals all 21 TFs internally; only these closures turn a sealed bucket into a persisted `BufferedSeal`. **Skip D1 here** (not in the aggregator loops) → keeps the fixed 21-slot arrays + the `42 = 2×21` aggregator unit test intact.
2. **prev-OI reads OI from `candles_1d`** at boot (`main.rs:3021` `load_from_questdb`) AND every midnight (`main.rs:3159` `spawn_prev_oi_cache_refresh_task` → `tick_enricher.rs:214` → `prev_oi_cache.rs:176` `SELECT … oi FROM candles_1d`). If we stop sealing D1 without redirecting this, every `oi_pct_from_prev_day` silently becomes 0.0 → CRITICAL silent corruption. (Boot ALSO loads prev-OI from Option Chain REST at `main.rs:2933`; the candles_1d path is an additional/overlapping source.)
3. **`prev_day_ohlcv` has NO `oi` column** (`prev_day_ohlcv_persistence.rs` — open/high/low/close/volume only). The boot fetch (`prev_day_ohlcv_boot.rs`) currently does NOT request OI from Dhan. **`grep "FROM prev_day_ohlcv"` = 0 hits** — the table is write-only; no reader exists yet.
4. Only `bar_cache_loader.rs` actively SELECTs `candles_1d_shadow`; would load 0 D1 rows. Other "candles_1d" references in tick_enricher/prev_oi_cache/boot_ordering_gate doc comments are about the OI path above.

## Plan Items (ALL land in ONE PR — partial = OI% corruption)

- [ ] **Storage: add `oi` to `prev_day_ohlcv`**
  - Files: crates/storage/src/prev_day_ohlcv_persistence.rs
  - DDL gets `oi LONG`; `PrevDayOhlcvRow` gets `pub oi: i64`; ILP append adds `column_i64("oi", ...)`. DEDUP key unchanged.
  - Tests: prev_day_ohlcv_create_ddl test asserts `oi` column.

- [ ] **Fetch: request + parse OI in the daily pull**
  - Files: crates/app/src/prev_day_ohlcv_boot.rs
  - Add `oi: true` to the Dhan `/v2/charts/historical` request; parse the `open_interest` columnar array (per historical-data.md); set `row.oi` (0 for equities/indices where OI is meaningless — Dhan returns all-zero).
  - Tests: parse test for the oi array.

- [ ] **Repoint prev-OI from candles_1d → prev_day_ohlcv**
  - Files: crates/core/src/pipeline/prev_oi_cache.rs:176
  - SQL `FROM candles_1d` → `FROM prev_day_ohlcv`. Same columns (security_id, segment, oi), same LATEST-ON semantics.
  - Tests: phase2_11_prev_oi_refresh.rs + phase2_7 → assert reload from prev_day_ohlcv.

- [ ] **Aggregator: skip D1 seal emission**
  - Files: crates/app/src/main.rs (~6003 + ~6123)
  - `if tf == TfIndex::D1 { return; }` as the first line of each on_seal closure.
  - Tests: aggregator unit tests UNCHANGED (skip is in main.rs, not the aggregator).

- [ ] **bar_cache_loader: drop the 1d tuple** (1d now from prev_day_ohlcv, not candles_1d_shadow)
  - Files: crates/app/src/bar_cache_loader.rs
  - Tests: bar_cache_loader tests (721/761) drop `1d`.

- [ ] **Rule + ratchets**
  - live-feed-purity.md: one-line carve-out — "candles_1d is historical-only (prev_day_ohlcv); the live aggregator does NOT seal D1."
  - metrics_catalog.rs: exempt D1 from market-hours seal-rate expectations (avoid false "broken" alarm per audit Rule 11).

## Edge cases (covered)
- prev_day_ohlcv empty at boot (Dhan fetch failed) → prev-OI loads 0 → add a positive-signal log so it's not a silent false-OK (audit Rule 11).
- Today's 1d absent until next morning's pull → confirmed no consumer requires same-day D1.

## Keep unchanged
`TfIndex::D1`, `TF_COUNT=21`, `candles_1d` DDL/self-heal, the ordinal-indexed `[Sender;21]` — reordering = silent SEMVER break (`tf_index.rs` docs). The table stays created (just unwritten by ticks).
