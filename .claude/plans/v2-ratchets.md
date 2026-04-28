# Movers 22-TF v3 — Ratchets (47 total + 3 banned-pattern + 2 make-doctor + 1 chaos)

## 47 mechanical ratchets

### DDL & Schema completeness (10) — unchanged from v2

| # | Test | Severity |
|---|---|---|
| 1 | `test_22_movers_tables_all_exist_at_boot` | CRITICAL |
| 2 | `test_movers_table_schema_uniform_across_all_22` | CRITICAL |
| 3 | `test_dedup_keys_include_segment_for_all_22_tables` | CRITICAL |
| 4 | `test_movers_tables_partition_by_day_with_wal` | HIGH |
| 5 | `test_movers_column_types_match_schema_spec_26_columns` | HIGH |
| 6 | `test_movers_segment_symbol_nocache_others_sized` | MEDIUM |
| 7 | `test_movers_partition_manager_includes_22_tables` | CRITICAL |
| 8 | `test_movers_s3_lifecycle_includes_22_tables` | HIGH |
| 9 | `test_movers_schema_self_heal_idempotent` | HIGH |
| 10 | `test_movers_22tf_constants_22_entries` | CRITICAL |

### Hot-path latency + dedup (8) — unchanged from v2

| # | Test | Severity |
|---|---|---|
| 11 | `test_upsert_idempotency_same_key_collapses_to_one_row` | CRITICAL |
| 12 | `test_mover_row_is_copy_zero_alloc_hot_path` (≤296 B) | HIGH |
| 13 | `test_movers_tracker_record_tick_o1_latency` (<500ns p99) | HIGH |
| 14 | `test_movers_writer_one_bad_timeframe_does_not_poison_others` | MEDIUM |
| 15 | `test_movers_writer_full_mpsc_increments_prometheus_counter` | MEDIUM |
| 16 | `dhat_movers_22tf_record_tick_zero_alloc` (10K ticks, 0 blocks) | CRITICAL |
| 17 | `dhat_movers_22tf_snapshot_24k_zero_alloc` | CRITICAL |
| 18 | `test_movers_writer_uses_papaya_not_hashmap_for_shared_state` | HIGH |

### Snapshot orchestration + cadence (6) — unchanged from v2

| # | Test | Severity |
|---|---|---|
| 19 | `test_movers_22tf_scheduler_spawns_all_tasks` | CRITICAL |
| 20 | `test_movers_snapshot_cadence_fires_within_tolerance` | HIGH |
| 21 | `test_movers_snapshot_universe_coverage_per_segment` | CRITICAL |
| 22 | `test_movers_per_segment_all_segments_write_rows` | HIGH |
| 23 | `test_movers_snapshot_task_panic_is_respawned` | MEDIUM |
| 24 | `test_movers_scheduler_respects_market_hours_gate` | HIGH |

### Segment-aware compute routing (5) — unchanged from v2

| # | Test | Severity |
|---|---|---|
| 25 | `test_movers_idx_i_prev_close_from_in_memory_cache` | CRITICAL |
| 26 | `test_movers_nse_eq_prev_close_from_tick_day_close` | CRITICAL |
| 27 | `test_movers_nse_fno_prev_close_from_tick_day_close` | CRITICAL |
| 28 | `test_movers_same_id_different_segment_both_write` (I-P1-11) | CRITICAL |
| 29 | `test_movers_first_seen_set_resets_at_ist_midnight` | HIGH |

### Materialized view integrity (4) — unchanged from v2

| # | Test | Severity |
|---|---|---|
| 30 | `test_27_candle_materialized_views_all_exist` | HIGH |
| 31 | `test_candle_view_bucket_offset_alignment` | MEDIUM |
| 32 | `test_movers_materialized_views_source_from_candles_1m` | HIGH |
| 33 | `test_candle_view_columns_match_source_schema` | MEDIUM |

### Prometheus + Telegram + runbook (4) — unchanged from v2

| # | Test | Severity |
|---|---|---|
| 34 | `test_movers_prometheus_metrics_all_emitted` | HIGH |
| 35 | `test_movers_timeframe_label_cardinality_22` | HIGH |
| 36 | `test_movers_22tf_error_codes_defined_in_enum` | CRITICAL |
| 37 | `test_movers_22tf_triage_rules_referenced` | CRITICAL |

### NEW — Dhan UI tab queries (4)

| # | Test | Pins |
|---|---|---|
| 38 | `test_stocks_price_movers_gainers_losers_query` | Stocks tab returns 50-row sorted result by change_pct |
| 39 | `test_options_seven_tabs_queries` | All 7 options tabs (Highest OI, OI Gainers/Losers, Top Volume/Value, Price Gainers/Losers) return correct sort order |
| 40 | `test_futures_seven_tabs_queries_premium_discount` | All 7 futures tabs incl. `(ltp - spot_price)` premium calculation |
| 41 | `test_options_futures_expiry_month_dropdown_returns_distinct_dates` | Dropdown source query returns unique sorted future expiry_dates |

### NEW — Depth integration (3)

| # | Test | Pins |
|---|---|---|
| 42 | `test_movers_depth_columns_populated_for_subscribed_instruments` | NSE_EQ + NIFTY/BANKNIFTY ATM options have non-null depth columns |
| 43 | `test_movers_depth_columns_null_for_unsubscribed_instruments` | Stock options (no depth-20) → NULL depth columns, no crash |
| 44 | `test_dashboard_depth_panels_query_existing_market_depth_table` | 6 depth panels point to `market_depth`/`deep_market_depth`, NOT new tables |

### NEW — Stock F&O expiry rollover T-only (3)

| # | Test | Pins |
|---|---|---|
| 45 | `test_stock_expiry_rollover_constant_is_zero` | `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS == 0` |
| 46 | `test_stock_expiry_keeps_nearest_on_t_minus_1` (REWRITE of existing) | Wed with Thu expiry → KEEP Thursday (was: roll) |
| 47 | `test_stock_expiry_rolls_only_on_t_zero` | Thursday IS Thursday-expiry → roll to next month |

### Existing ratchets to UPDATE (5 in `subscription_planner.rs::tests`)

| Existing test | Old assertion | New assertion |
|---|---|---|
| `test_stock_expiry_rolls_on_t_minus_1` | rolls when T-1 | RENAME to `test_stock_expiry_keeps_nearest_on_t_minus_1` (KEEPS) |
| `test_stock_expiry_rolls_on_t` | rolls when T-0 | RENAME to `test_stock_expiry_rolls_only_on_t_zero` (unchanged behavior) |
| `test_stock_expiry_stays_on_t_minus_2` | KEEPS at T-2 | unchanged |
| `test_index_expiry_never_rolls_via_planner` | indices keep nearest | unchanged |
| `test_count_trading_days_expiry_day_from_t_minus_1` | helper test | unchanged |

## 3 banned-pattern hook additions

| # | Cat | Pattern | Block condition |
|---|---|---|---|
| 8 | Movers I-P1-11 | `HashMap<u32,` not `(u32, ExchangeSegment)` | Reject in movers paths without composite or `// APPROVED:` |
| 9 | Movers DDL registration | `CREATE TABLE IF NOT EXISTS movers_` not in `MOVERS_TIMEFRAMES` | Reject orphan tables |
| 10 | Movers writer panic | `.unwrap()` / `.expect(` in movers persistence | Reject without `// O(1) EXEMPT:` |

## 2 `make doctor` sections

| # | Section | Checks |
|---|---|---|
| 8 | Movers DDL Integrity | 22 tables exist; DEDUP keys; columns uniform |
| 9 | Movers Universe Coverage | `tv_movers_universe_size ≥ 24K`; 4 distinct segments |

## 1 chaos test (merge-blocker)

`crates/storage/tests/chaos_movers_22tf_throughput.rs` — ≥1M rows/sec sustained, ≤0.1% drop rate, no ILP RST. Tagged `#[ignore]` default; CI nightly via `CHAOS=1`.

## Existing ratchets to extend (4)

| Test | Extension |
|---|---|
| `dedup_segment_meta_guard.rs` | Add 22 movers tables |
| `triage_rules_guard.rs` | Add `MOVERS-22TF-*` entries |
| `recording_rules_guard.rs` | Add movers-22tf alert rules |
| `zero_tick_loss_alert_guard.rs` | Check `movers-22tf.json` panel UIDs (<256 chars) |

## Test count ratchet impact

47 new + 5 rewritten + 2 DHAT + 3 Criterion + 1 chaos = **+58 net test moves**. Test-count ratchet only-increases.

## Coverage thresholds (100% per `crate-coverage-thresholds.toml`)

- `crates/common/src/mover_types.rs`
- `crates/core/src/pipeline/movers_22tf_scheduler.rs`
- `crates/core/src/pipeline/movers_22tf_writer_state.rs`
- `crates/core/src/pipeline/movers_22tf_supervisor.rs`
- `crates/storage/src/movers_22tf_persistence.rs`

## 7-layer observability per code path

| # | Layer | Mover artefact |
|---|---|---|
| 1 | Prometheus counter | 10 `tv_movers_*_total` |
| 2 | Prometheus gauge | `tv_movers_universe_size`, `_per_segment_count`, `_snapshot_lag_seconds` |
| 3 | Tracing span | `#[instrument]` on `snapshot_into` |
| 4 | Loki log | `error!(code = ErrorCode::Movers22Tf01.code_str(), ...)` |
| 5 | Telegram | `NotificationEvent::Movers22TfWriterFailed` (Severity::High) |
| 6 | Grafana | 24 panels with `increase()` / `rate()` (Stocks 2 + Index 2 + Options 7 + Futures 7 + Depth 6) |
| 7 | Audit | piggyback `phase2_audit.diagnostic` JSON |
