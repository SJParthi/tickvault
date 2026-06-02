# Active Plan: 1d timeframe = historical-only (never tick-calculated)

**Status:** VERIFIED — all 6 items implemented + scoped tests green; PR open, monitoring to merge
**Date:** 2026-06-02
**Approved by:** Parthiban — "1 day timeframe should never ever be calculated, always pulled once and only from historical 1 day timeframe" + chose **"Add OI to the daily pull"** when asked how prev-OI sources its OI.

## Verified facts (from 3-agent adversarial review + my own code verification)

1. **The write boundary** for `candles_1d` is TWO `on_seal` closures in `crates/app/src/main.rs` (~line 6003 per-tick + ~6123 boundary force-seal). The aggregator (`multi_tf_aggregator.rs`) still seals all 21 TFs internally; only these closures turn a sealed bucket into a persisted `BufferedSeal`. **Skip D1 here** (not in the aggregator loops) → keeps the fixed 21-slot arrays + the `42 = 2×21` aggregator unit test intact.
2. **prev-OI reads OI from `candles_1d`** at boot (`main.rs:3021` `load_from_questdb`) AND every midnight (`main.rs:3159` `spawn_prev_oi_cache_refresh_task` → `tick_enricher.rs:214` → `prev_oi_cache.rs:176` `SELECT … oi FROM candles_1d`). If we stop sealing D1 without redirecting this, every `oi_pct_from_prev_day` silently becomes 0.0 → CRITICAL silent corruption. (Boot ALSO loads prev-OI from Option Chain REST at `main.rs:2933`; the candles_1d path is an additional/overlapping source.)
3. **`prev_day_ohlcv` has NO `oi` column** (`prev_day_ohlcv_persistence.rs` — open/high/low/close/volume only). The boot fetch (`prev_day_ohlcv_boot.rs`) currently does NOT request OI from Dhan. **`grep "FROM prev_day_ohlcv"` = 0 hits** — the table is write-only; no reader exists yet.
4. Only `bar_cache_loader.rs` actively SELECTs `candles_1d_shadow`; would load 0 D1 rows. Other "candles_1d" references in tick_enricher/prev_oi_cache/boot_ordering_gate doc comments are about the OI path above.

## Plan Items (ALL land in ONE PR — partial = OI% corruption)

- [x] **Storage: add `oi` to `prev_day_ohlcv`**
  - Files: crates/storage/src/prev_day_ohlcv_persistence.rs
  - DDL gets `oi LONG`; `PrevDayOhlcvRow` gets `pub oi: i64`; ILP append adds `column_i64("oi", ...)`; added idempotent `prev_day_ohlcv_alter_add_oi_ddl()` schema self-heal. DEDUP key unchanged.
  - Result: 9/9 storage tests green.
  - Tests: test_prev_day_ohlcv_create_ddl_has_all_columns, test_prev_day_ohlcv_alter_add_oi_ddl_is_idempotent_add_column, test_append_row_serialises_nonzero_oi_for_fno

- [x] **Fetch: request + parse OI in the daily pull**
  - Files: crates/app/src/prev_day_ohlcv_boot.rs
  - Added `oi: true` to the Dhan request; parse `open_interest` columnar array defensively (absent/short → 0, never rejects the candle); `DailyCandle.oi` → `PrevDayOhlcvRow.oi`.
  - Result: 16/16 app tests green.
  - Tests: parse_open_interest_for_fno, parse_open_interest_takes_last_when_multiple, parse_open_interest_length_mismatch_defaults_zero, parse_open_interest_as_float_truncates

- [x] **Repoint prev-OI from candles_1d → prev_day_ohlcv**
  - Files: crates/core/src/pipeline/prev_oi_cache.rs
  - SQL `FROM candles_1d` → `FROM prev_day_ohlcv`; window widened `-2d` → `-10d` (prev-day row sits at IST-midnight `ts`, several calendar days back over a long weekend — `-2d` would have EXCLUDED Friday's row on a Monday boot). Doc comments + log lines updated.
  - Result: prev_oi_cache 23/23 green; phase2_11 + phase2_7 source-scans updated and green.
  - Tests: hostile_c1_midnight_rollover_task_spawned_in_slow_boot, hot_path_c2_now_ist_secs_hoisted_to_frame_level, phase2_11_emits_three_outcome_labels_for_observability

- [x] **Aggregator: skip D1 seal emission**
  - Files: crates/app/src/main.rs (per-tick + IST-midnight force-seal closures)
  - `if tf == TfIndex::D1 { return; }` as the first line of each closure; added `TfIndex` to the use-import. Aggregator still seals 21 TFs internally (TF_COUNT=21, scale-guard green).

- [x] **bar_cache_loader: drop the 1d tuple** (1d now from prev_day_ohlcv)
  - Files: crates/app/src/bar_cache_loader.rs
  - Dropped `candles_1d_shadow` tuple (9→8 tables); tests renamed/updated to assert exclusion. Result: 19/19 green.
  - Tests: test_build_sql_includes_all_8_shadow_tables, test_build_sql_uses_union_all_between_tables, test_build_sql_injects_tf_label_literals

- [x] **Rule + ratchets**
  - live-feed-purity.md: added **rule 10** — full "1d historical-only" carve-out (aggregator seals 21 internally; main.rs write boundary drops D1; candles_1d unwritten; prev_day_ohlcv is the 1d source).
  - Fixed a PRE-EXISTING stale ratchet `hot_path_c2_now_ist_secs_hoisted_to_frame_level` (was red on `main` — asserted removed `frame_now_ist_secs`; updated to pin the current stronger `tick.exchange_timestamp % 86_400` invariant).


## Edge cases (covered)
- prev_day_ohlcv empty at boot (Dhan fetch failed) → prev-OI loads 0 → positive-signal log so it's not a silent false-OK (audit Rule 11).
- Today's 1d absent until next morning's pull → confirmed no consumer requires same-day D1.

## Keep unchanged
`TfIndex::D1`, `TF_COUNT=21`, `candles_1d` DDL/self-heal, the ordinal-indexed `[Sender;21]` — reordering = silent SEMVER break. The table stays created (just unwritten by ticks).

---

## NEXT PR (operator request 2026-06-02)
Post-market 1-minute cross-verification — full design, pros/cons, and open questions are in
**`.claude/plans/followup-cross-verify-1m.md`** (DRAFT). Implement AFTER this 1d PR merges
(serial-PR protocol, operator-charter §H).
