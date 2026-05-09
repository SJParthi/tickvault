# Movers Infrastructure Cleanup — Roadmap (post-PR #529)

**Status:** DRAFT plan — no code change in this PR.
**Authority:** plan §P operator directive 2026-05-08 — *"remove all movers related infrastructure entirely. We have ticks and candles alone — that's it."*
**Authored:** 2026-05-09 (after PR #529 V1 handler deletion landed).

This document captures the deletion sequence for the remaining movers
infrastructure so a future session (or the same operator on a non-blind
boot) can tackle each step with the right verification.

---

## Inventory — what's still alive after PR #529

| Component | File | Active reader / writer | Why it's still alive |
|---|---|---|---|
| `movers_questdb.rs` (V1 SQL helpers) | `crates/api/src/handlers/movers_questdb.rs` | Imported by `market_data::get_stock_movers` + `get_option_movers` | Two static dashboards still call `/api/market/stock-movers` |
| `market_data::get_stock_movers` | `crates/api/src/handlers/market_data.rs` | Static `markets-stocks.html` + `market-dashboard.html` | Same as above |
| `market_data::get_option_movers` | same | Static `markets-options.html` | Same — not migrated yet |
| `movers_writer` + `movers_unified_writer` | `crates/storage/src/` | `movers_unified_pipeline` task | Populates `movers_1s` matview chain |
| `movers_unified_pipeline` task | `crates/app/src/movers_unified_pipeline.rs` | Boot wiring | Depth-dynamic Stage 1 SQL reads `movers_1m` |
| 25 `movers_*` matviews | QuestDB DDL in `materialized_views.rs` | Read by `depth_dynamic_top_volume_selector::build_cohort_sql` | Hot-path SQL on every 60s rebalance |

---

## Step 1 — PR 5b1 (BACKEND IDENTITY) — required before frontend migration

**Problem (verified 2026-05-09):** `/api/movers/v2`'s response struct `MoversV2Bar` carries OHLCV + bucket timestamps but NO `security_id` / `exchange_segment` / `symbol`. The frontend dashboards need `symbol` to render table rows; V2 cannot supply that today.

**Scope:**

1. Extend `MoversV2Bar` (in `crates/api/src/handlers/movers_v2.rs`) with:
   - `security_id: u32`
   - `exchange_segment: u8` (or `String` for the symbol form)
2. Extend the `Bar` struct in `crates/trading/src/candles/mod.rs` if it doesn't already carry identity (verify first — likely already keyed by `(security_id, exchange_segment)` upstream).
3. Plumb identity through `top_n_by_bars` selector in `crates/trading/src/in_mem/top_n.rs` so the returned `Vec<Bar>` carries identity.
4. Update `MoversV2TopNResponse` ratchet test to assert identity fields are non-zero.
5. Backwards-compat: V2 is dormant by default (`config.api.movers_v2_enabled = false`), so no live consumer breaks during this expansion.

**Risk:** LOW — purely additive on a dormant endpoint. No frontend touched. No deletion.

**Verification:** `cargo test -p tickvault-api --lib`, `cargo test -p tickvault-trading --lib`, eyeball the `MoversV2TopNResponse` JSON against a curl probe in dev.

---

## Step 2 — PR 5b2 (FRONTEND MIGRATION)

**Scope:**

1. Rewrite the JS in `crates/api/static/markets-stocks.html` to fetch `/api/movers/v2?category=price_gainers&scope=stock&n=20&timeframe=1m` (and `price_losers`, `top_volume`).
2. Map the new `MoversV2Bar`-with-identity response to the existing `{ symbol, ltp, change, change_pct, volume }` row shape the renderer consumes.
3. Same for `markets-options.html` (option-movers analogue).
4. Same for `market-dashboard.html`.
5. Delete the 3 routes in `crates/api/src/lib.rs` (`/api/market/stock-movers`, `/api/market/option-movers`, plus any sibling).
6. Delete `crates/api/src/handlers/movers_questdb.rs` + `pub mod movers_questdb` line in `mod.rs`.
7. Delete `market_data::get_stock_movers` + `get_option_movers` (keep `get_indices`).

**Risk:** MEDIUM — frontend rewrite requires visual verification.

**Verification:** local `make run` + browser check of all 3 dashboards rendering correctly. Confirm gainers/losers/volume rows match Dhan UI.

---

## Step 3 — PR 5c (DEPTH-DYNAMIC RAM COHORT REWIRE)

**Scope:**

1. Add `CascadeFanout::top_n_by_volume(timeframe, scope, n)` accessor returning `Vec<Bar>` from RAM. Mirrors `top_n_by_bars` but pre-filters to `Category::TopVolume` semantics.
2. Replace `depth_dynamic_top_volume_selector::build_cohort_sql` with a RAM call into the new accessor.
3. Delete `crates/storage/src/movers_writer.rs` + `movers_unified_writer.rs` + `movers_unified_persistence.rs`.
4. Delete `crates/app/src/movers_unified_pipeline.rs` + boot wiring in `main.rs`.
5. Delete the `movers_5s` / `movers_1s` ILP write path and its dedup constants.

**Risk:** HIGH — depth-dynamic reads `movers_1m` every 60s; getting the RAM accessor wrong silently swaps subscriptions to the wrong contracts.

**Verification:** loom concurrency test on `top_n_by_volume`; chaos test asserting depth-20/depth-200 selector returns the same top-N before vs after rewire on a recorded tick replay.

---

## Step 4 — PR 5d (MATVIEW DDL DROP)

**Scope:**

1. Remove the 25 `movers_*` entries from `VIEW_DEFS` in `crates/storage/src/materialized_views.rs`.
2. Add the 25 names to `BUG3_RETIRED_MATERIALIZED_VIEWS` const + `drop_bug3_retired_views` boot DDL function (mirroring the `TF_REDUCTION_RETIRED_MATERIALIZED_VIEWS` pattern from PR #525).
3. Update grafana_query_tests' `MATERIALIZED_VIEWS` list.
4. Update `view_count_is_*` ratchet.

**Risk:** LOW — by Step 4, no reader exists; matview drop is mechanical.

**Verification:** boot the app, confirm the boot log shows `drop_bug3_retired_views` fired and the operator's QuestDB sidebar no longer lists `movers_*`.

---

## Why this can't be a single session

Each step's verification gate is independent:

- **5b1** — pure backend, can be CI-verified.
- **5b2** — needs a browser open at the operator's dev mac to click through 3 dashboards.
- **5c** — needs market-hours soak to confirm depth-dynamic still picks the right contracts.
- **5d** — needs one boot to confirm the DDL drop fires.

Attempting them blind in one PR risks invisible regressions that only surface at the next market open. The 5a slice already shipped (PR #529) is the safe envelope of what fits without operator presence.

## Cross-references

- Plan §P (in-memory store + AWS instance design): `removed all movers infrastructure` directive.
- PR #519 / #520 / #521 / #522: Wave-5 §K-L# baseline that retired the cascade engines.
- PR #525: 7-matview retirement — the pattern this roadmap mirrors.
- PR #529: V1 `/api/movers` handler deletion (the first slice).
