# Complete Ratchet List — 37 Mechanical Guards for No-Drift Guarantee

**Branch:** main (post #408 merge)
**Date:** 2026-04-28
**Agent:** Explore

## Executive Summary

The plan proposed 12 ratchets but needs **37 total mechanical guards** to guarantee the 67-table movers design cannot drift. Plus 3 banned-pattern hook additions and 2 `make doctor` sections.

## Categories

- **10 DDL/Schema completeness ratchets**
- **8 Hot-path latency + dedup correctness ratchets**
- **6 Snapshot orchestration + cadence ratchets**
- **5 Segment-aware compute routing ratchets**
- **4 Materialized view integrity ratchets**
- **2 Prometheus + Telegram reliability ratchets**
- **2 Operator runbook + doctor ratchets**

## Full Ratchet Test Table — 37 entries

### DDL & Schema Completeness (10)

| # | Test | Pins | Severity |
|---|---|---|---|
| 1 | `test_22_movers_tables_all_exist_at_boot` | All 22 tables exist; names match MOVERS_TIMEFRAMES | CRITICAL |
| 2 | `test_movers_table_schema_uniform_across_all_22` | All 22 tables identical column names + types | CRITICAL |
| 3 | `test_dedup_keys_include_segment_for_all_22_tables` | I-P1-11: every DEDUP key includes (ts, security_id, segment) | CRITICAL |
| 4 | `test_movers_tables_partition_by_day_with_wal` | All 22 use PARTITION BY DAY WAL | HIGH |
| 5 | `test_movers_column_types_match_schema_spec` | ts=TIMESTAMP, sec_id=INT, segment=SYMBOL, prices=DOUBLE, qtys=LONG | HIGH |
| 6 | `test_movers_segment_symbol_nocache_others_sized` | segment NOCACHE, underlying_symbol CACHE 1024 | MEDIUM |
| 7 | `test_movers_partition_manager_includes_22_tables` | partition_manager.rs HOUR_PARTITIONED_TABLES has all 22 | CRITICAL |
| 8 | `test_movers_s3_lifecycle_includes_22_tables` | s3-lifecycle-audit-tables.json or similar covers movers | HIGH |
| 9 | `test_movers_schema_self_heal_idempotent` | Boot twice with same DDL, no errors | HIGH |
| 10 | `test_movers_22tf_constants_22_entries` | MOVERS_TIMEFRAMES.len() == 22 with exact ordering | CRITICAL |

### Hot-path Latency + Dedup Correctness (8)

| # | Test | Pins | Severity |
|---|---|---|---|
| 11 | `test_upsert_idempotency_same_key_collapses_to_one_row` | Two writes same (ts,id,seg) → 1 row | CRITICAL |
| 12 | `test_mover_row_is_copy_zero_alloc_hot_path` | MoverRow is Copy, sizeof ≤ 192 B (3 cache lines) | HIGH |
| 13 | `test_movers_tracker_record_tick_o1_latency` | record_tick + update_prev_close < 500ns p99 | HIGH |
| 14 | `test_movers_writer_one_bad_timeframe_does_not_poison_others` | snapshot_1s ILP fail → snapshot_5s still completes | MEDIUM |
| 15 | `test_movers_writer_full_mpsc_increments_prometheus_counter` | mpsc full → tv_movers_writer_dropped_total++ | MEDIUM |
| 16 | `dhat_movers_22tf_record_tick_zero_alloc` | DHAT total_blocks == 0 over 10K ticks | CRITICAL |
| 17 | `dhat_movers_22tf_snapshot_24k_zero_alloc` | DHAT snapshot uses arena, no per-row alloc | CRITICAL |
| 18 | `test_movers_writer_uses_papaya_not_hashmap_for_shared_state` | Multi-reader concurrent state must use papaya | HIGH |

### Snapshot Orchestration + Cadence (6)

| # | Test | Pins | Severity |
|---|---|---|---|
| 19 | `test_movers_22tf_scheduler_spawns_all_tasks` | Exactly 22 tokio tasks spawn | CRITICAL |
| 20 | `test_movers_snapshot_cadence_fires_within_tolerance_1s_5s_1m` | Each task fires at T ± 50ms | HIGH |
| 21 | `test_movers_snapshot_universe_coverage_per_segment` | Every snapshot writes ≥3 indices, ≥200 NSE_EQ, ≥22000 NSE_FNO | CRITICAL |
| 22 | `test_movers_per_segment_all_segments_write_rows` | All 4 segments present in every snapshot | HIGH |
| 23 | `test_movers_snapshot_task_panic_is_respawned` | Pool supervisor respawns within 5s | MEDIUM |
| 24 | `test_movers_scheduler_respects_market_hours_gate` | No snapshot fires outside [09:00, 15:30] IST | HIGH |

### Segment-aware Prev_close Compute Routing (5)

| # | Test | Pins | Severity |
|---|---|---|---|
| 25 | `test_movers_idx_i_prev_close_from_in_memory_cache` | IDX_I uses cache, not tick.day_close | CRITICAL |
| 26 | `test_movers_nse_eq_prev_close_from_tick_day_close` | NSE_EQ uses tick.day_close field directly | CRITICAL |
| 27 | `test_movers_nse_fno_prev_close_from_tick_day_close` | NSE_FNO uses tick.day_close field directly | CRITICAL |
| 28 | `test_movers_same_id_different_segment_both_write` | NIFTY (IDX_I,id=13) + stock (NSE_EQ,id=13) both rows present | CRITICAL |
| 29 | `test_movers_first_seen_set_resets_at_ist_midnight` | First-Quote/Full per (id,seg) per day re-fires after IST 00:00 | HIGH |

### Materialized View Integrity (4)

| # | Test | Pins | Severity |
|---|---|---|---|
| 30 | `test_27_candle_materialized_views_all_exist` | 18 + 9 = 27 views | HIGH |
| 31 | `test_candle_view_bucket_offset_alignment` | 1s-15m use OFFSET '00:00'; 30m/1h use '00:15' | MEDIUM |
| 32 | `test_movers_materialized_views_source_from_candles_1m` | All 9 new views FROM candles_1m | HIGH |
| 33 | `test_candle_view_columns_match_source_schema` | Every view has identical columns to source | MEDIUM |

### Prometheus + Telegram Reliability (2)

| # | Test | Pins | Severity |
|---|---|---|---|
| 34 | `test_movers_prometheus_metrics_all_emitted` | 10 new metrics registered after 1 snapshot | HIGH |
| 35 | `test_movers_timeframe_label_cardinality_22` | Metrics with {timeframe} have exactly 22 distinct labels | HIGH |

### Operator Runbook + Doctor (2)

| # | Test | Pins | Severity |
|---|---|---|---|
| 36 | `test_movers_22tf_error_codes_defined_in_enum` | ErrorCode enum has MOVERS-22TF-01/02/03 | CRITICAL |
| 37 | `test_movers_22tf_triage_rules_referenced` | Triage YAML covers all 3 codes + runbook path exists | CRITICAL |

## Banned-Pattern Hook Additions (3 new categories)

| # | Cat | Pattern | Block condition |
|---|---|---|---|
| 8 | Movers I-P1-11 | `HashMap<u32,` not in `(u32, ExchangeSegment)` | Reject in movers paths without composite key OR `// APPROVED:` comment |
| 9 | Movers DDL registration | `CREATE TABLE IF NOT EXISTS movers_` without entry in MOVERS_TIMEFRAMES | Reject orphan movers tables |
| 10 | Movers writer panic | `.unwrap()` / `.expect(` in movers persistence | Reject on hot path without `// O(1) EXEMPT:` comment |

## `make doctor` Additions (2 new sections)

| # | Section | Checks |
|---|---|---|
| 8 | Movers DDL Integrity | 22 tables exist; DEDUP keys present; columns uniform |
| 9 | Movers Universe Coverage | tv_movers_universe_size ≥ 24K (or 0 if market closed); 4 distinct segments present |

## Existing Ratchets to Extend

| Test | What | Extension |
|---|---|---|
| `dedup_segment_meta_guard.rs` | I-P1-11 segment in DEDUP keys | Add 22 movers tables |
| `triage_rules_guard.rs` | Triage YAML structure | Add MOVERS-22TF-* entries |
| `recording_rules_guard.rs` | Prometheus alert syntax | Add movers-22tf alerts |
| `zero_tick_loss_alert_guard.rs` | Grafana UID < 256 chars | Check movers-22tf.json panel UIDs |

## Coverage Thresholds

100% required for new modules:
- `crates/common/src/mover_types.rs`
- `crates/core/src/pipeline/movers_22tf_scheduler.rs`
- `crates/core/src/pipeline/movers_22tf_writer_state.rs`
- `crates/storage/src/movers_22tf_persistence.rs`

## Test Count Ratchet Impact

Current baseline + 37 new = new baseline. Ratchet only increases.

## Summary

- **37 mechanical guards** (vs 12 in plan = +25)
- **3 new banned-pattern rules**
- **2 new `make doctor` sections**
- **Every invariant has a BLOCKING test** (exit 0 only if all pass)
- Build fails on any drift
