# Movers 22-TF v2 — Ratchets (37 + 3 banned-pattern + 2 make-doctor + 1 chaos)

## 37 mechanical ratchets

### DDL & Schema completeness (10)

| # | Test | Pins | Severity |
|---|---|---|---|
| 1 | `test_22_movers_tables_all_exist_at_boot` | All 22 tables exist; names match `MOVERS_TIMEFRAMES` | CRITICAL |
| 2 | `test_movers_table_schema_uniform_across_all_22` | All 22 identical column names + types | CRITICAL |
| 3 | `test_dedup_keys_include_segment_for_all_22_tables` | I-P1-11: every DEDUP key includes `(ts, security_id, segment)` | CRITICAL |
| 4 | `test_movers_tables_partition_by_day_with_wal` | All 22 use `PARTITION BY DAY WAL` | HIGH |
| 5 | `test_movers_column_types_match_schema_spec` | `ts=TIMESTAMP, sec_id=INT, segment=SYMBOL, prices=DOUBLE, qtys=LONG` | HIGH |
| 6 | `test_movers_segment_symbol_nocache_others_sized` | `segment NOCACHE`, `underlying_symbol CACHE 1024` | MEDIUM |
| 7 | `test_movers_partition_manager_includes_22_tables` | `partition_manager.rs::HOUR_PARTITIONED_TABLES` has all 22 (closes Risk #7) | CRITICAL |
| 8 | `test_movers_s3_lifecycle_includes_22_tables` | `s3-lifecycle-movers-tables.json` covers all 22 (closes Risk #8) | HIGH |
| 9 | `test_movers_schema_self_heal_idempotent` | Boot twice with same DDL, no errors | HIGH |
| 10 | `test_movers_22tf_constants_22_entries` | `MOVERS_TIMEFRAMES.len() == 22` with exact ordering | CRITICAL |

### Hot-path latency + dedup correctness (8)

| # | Test | Pins | Severity |
|---|---|---|---|
| 11 | `test_upsert_idempotency_same_key_collapses_to_one_row` | Two writes same `(ts,id,seg)` → 1 row | CRITICAL |
| 12 | `test_mover_row_is_copy_zero_alloc_hot_path` | `MoverRow: Copy`, `sizeof ≤ 192 B` | HIGH |
| 13 | `test_movers_tracker_record_tick_o1_latency` | `record_tick + update_prev_close < 500 ns p99` | HIGH |
| 14 | `test_movers_writer_one_bad_timeframe_does_not_poison_others` | `snapshot_1s` ILP fail → `snapshot_5s` still completes | MEDIUM |
| 15 | `test_movers_writer_full_mpsc_increments_prometheus_counter` | mpsc full → `tv_movers_writer_dropped_total{reason="full_drop_newest"}++` | MEDIUM |
| 16 | `dhat_movers_22tf_record_tick_zero_alloc` | DHAT `total_blocks == 0` over 10K ticks | CRITICAL |
| 17 | `dhat_movers_22tf_snapshot_24k_zero_alloc` | Snapshot uses arena, no per-row alloc | CRITICAL |
| 18 | `test_movers_writer_uses_papaya_not_hashmap_for_shared_state` | Tracker shared state must be `papaya::HashMap`, NOT `std::HashMap` (closes 01 §B + §2) | HIGH |

### Snapshot orchestration + cadence (6)

| # | Test | Pins | Severity |
|---|---|---|---|
| 19 | `test_movers_22tf_scheduler_spawns_all_tasks` | Exactly 22 `tokio::spawn` futures spawn | CRITICAL |
| 20 | `test_movers_snapshot_cadence_fires_within_tolerance_1s_5s_1m` | Each task fires at `T ± 50 ms` | HIGH |
| 21 | `test_movers_snapshot_universe_coverage_per_segment` | Every snapshot writes ≥3 indices, ≥200 NSE_EQ, ≥22000 NSE_FNO (closes 01 §C) | CRITICAL |
| 22 | `test_movers_per_segment_all_segments_write_rows` | All 4 segments present in every snapshot | HIGH |
| 23 | `test_movers_snapshot_task_panic_is_respawned` | `MoversSupervisor` respawns within 5s (closes Risk #4) | MEDIUM |
| 24 | `test_movers_scheduler_respects_market_hours_gate` | No snapshot fires outside `[09:00, 15:30] IST` (closes 01 §A) | HIGH |

### Segment-aware compute routing (5)

| # | Test | Pins | Severity |
|---|---|---|---|
| 25 | `test_movers_idx_i_prev_close_from_in_memory_cache` | IDX_I uses cache, not `tick.day_close` | CRITICAL |
| 26 | `test_movers_nse_eq_prev_close_from_tick_day_close` | NSE_EQ uses `tick.day_close` directly | CRITICAL |
| 27 | `test_movers_nse_fno_prev_close_from_tick_day_close` | NSE_FNO uses `tick.day_close` directly | CRITICAL |
| 28 | `test_movers_same_id_different_segment_both_write` | NIFTY (IDX_I,id=13) + cash (NSE_EQ,id=13) both rows present (I-P1-11) | CRITICAL |
| 29 | `test_movers_first_seen_set_resets_at_ist_midnight` | First-Quote/Full per (id,seg) per day re-fires after IST 00:00 | HIGH |

### Materialized view integrity (4)

| # | Test | Pins | Severity |
|---|---|---|---|
| 30 | `test_27_candle_materialized_views_all_exist` | 18 + 9 = 27 views | HIGH |
| 31 | `test_candle_view_bucket_offset_alignment` | 1s-15m use `OFFSET '00:00'`; 30m/1h use `'00:15'` | MEDIUM |
| 32 | `test_movers_materialized_views_source_from_candles_1m` | All 9 new views FROM `candles_1m` | HIGH |
| 33 | `test_candle_view_columns_match_source_schema` | Every view has identical columns to source | MEDIUM |

### Prometheus + Telegram reliability (2)

| # | Test | Pins | Severity |
|---|---|---|---|
| 34 | `test_movers_prometheus_metrics_all_emitted` | 10 new metrics registered after 1 snapshot | HIGH |
| 35 | `test_movers_timeframe_label_cardinality_22` | Metrics with `{timeframe}` have exactly 22 distinct labels | HIGH |

### Operator runbook + doctor (2)

| # | Test | Pins | Severity |
|---|---|---|---|
| 36 | `test_movers_22tf_error_codes_defined_in_enum` | `ErrorCode` enum has `MOVERS-22TF-01/02/03` | CRITICAL |
| 37 | `test_movers_22tf_triage_rules_referenced` | Triage YAML covers all 3 codes + runbook path exists | CRITICAL |

## 3 banned-pattern hook additions

| # | Cat | Pattern | Block condition |
|---|---|---|---|
| 8 | Movers I-P1-11 | `HashMap<u32,` not `(u32, ExchangeSegment)` | Reject in movers paths without composite key OR `// APPROVED:` |
| 9 | Movers DDL registration | `CREATE TABLE IF NOT EXISTS movers_` not in `MOVERS_TIMEFRAMES` | Reject orphan tables |
| 10 | Movers writer panic | `.unwrap()` / `.expect(` in movers persistence | Reject without `// O(1) EXEMPT:` |

## 2 `make doctor` sections

| # | Section | Checks |
|---|---|---|
| 8 | Movers DDL Integrity | 22 tables exist; DEDUP keys present; columns uniform |
| 9 | Movers Universe Coverage | `tv_movers_universe_size ≥ 24K` (or 0 outside market hours); 4 distinct segments present |

## 1 chaos test (merge-blocker)

`crates/storage/tests/chaos_movers_22tf_throughput.rs` — see v2-risks.md “Unverified claim”.
- Sustains 22-writer 60s coordinated burst
- Asserts ≥1.0M rows/sec sustained, ≤0.1% drop rate, no ILP RST
- `#[ignore]` default; runs nightly via `CHAOS=1`

## Existing ratchets to extend (4)

| Test | Extension |
|---|---|
| `dedup_segment_meta_guard.rs` | Add 22 movers tables |
| `triage_rules_guard.rs` | Add `MOVERS-22TF-*` entries |
| `recording_rules_guard.rs` | Add movers-22tf alert rules |
| `zero_tick_loss_alert_guard.rs` | Check `movers-22tf.json` panel UIDs (<256 chars) |

## Test count ratchet impact

37 new ratchets + 2 DHAT + 3 Criterion + 1 chaos = **+43 tests**. Ratchet only-increases.

## Coverage thresholds (100% per `quality/crate-coverage-thresholds.toml`)

- `crates/common/src/mover_types.rs`
- `crates/core/src/pipeline/movers_22tf_scheduler.rs`
- `crates/core/src/pipeline/movers_22tf_writer_state.rs`
- `crates/core/src/pipeline/movers_22tf_supervisor.rs`
- `crates/storage/src/movers_22tf_persistence.rs`

## 7-layer observability per code path (per `wave-4-shared-preamble.md` §4)

| # | Layer | Mover artefact |
|---|---|---|
| 1 | Prometheus counter | 10 new `tv_movers_*_total` |
| 2 | Prometheus gauge | `tv_movers_universe_size`, `tv_movers_per_segment_count`, `tv_movers_snapshot_lag_seconds` |
| 3 | Tracing span | `#[instrument]` on `MoversTracker::snapshot_into` |
| 4 | Loki structured log | `error!(code = ErrorCode::Movers22Tf01.code_str(), ...)` |
| 5 | Telegram event | `NotificationEvent::Movers22TfWriterFailed` (Severity::High) |
| 6 | Grafana panel | `movers-22tf.json` 6 panels wrap counters in `increase()` per Rule 12 |
| 7 | Audit table | Piggyback `phase2_audit.diagnostic` JSON for daily Phase-2 row count summary (per audit 02 — no new audit table) |
