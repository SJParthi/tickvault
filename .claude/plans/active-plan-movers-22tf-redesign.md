# Movers @ 22-Timeframe Full-Universe Redesign — Implementation Plan

**Status:** DRAFT (pending Parthiban approval)
**Date:** 2026-04-28
**Branch:** `claude/code-review-check-HlB8c`
**Scope:** ~24K instruments × 22 timeframes (1s → 1h) movers compute + persist to QuestDB

## Goal

For **every** instrument in the live universe (indices, index F&O, stocks, stock F&O ≈ 24,324 total) compute `change_pct = (LTP - prev_day_close) / prev_day_close × 100` continuously on every tick (already done by `MoversTracker`), and **snapshot the full ranked state to QuestDB at 22 cadences**.

The 22 cadences = the same ladder as candle materialized views:

```
1s, 5s, 10s, 15s, 30s,
1m, 2m, 3m, 4m, 5m, 6m, 7m, 8m, 9m, 10m, 11m, 12m, 13m, 14m, 15m,
30m, 1h
```

## Prev_close source per segment (verified by 4-agent audit, 2026-04-28)

| Segment | Source | Code path |
|---|---|---|
| IDX_I (~30) | PrevClose code-6 packet → in-memory `HashMap<u32, f32>` cache (`tick_processor.rs:854 index_prev_close_cache`) + `previous_close` table for cross-restart resilience | `tick_processor.rs:1612` |
| NSE_EQ (~216) | `tick.day_close` field bytes 38-41 of every Quote packet OR bytes 50-53 of every Full packet (per Dhan ticket #5525125) — NO table query in hot path | `top_movers.rs:185` fallback path |
| NSE_FNO index F&O (~2,067) | `tick.day_close` field bytes 50-53 of every Full packet | same |
| NSE_FNO stock F&O (~22,011) | `tick.day_close` field bytes 50-53 of every Full packet | same |

**Hot-path lookup is O(1) via composite-key HashMap, zero QuestDB queries per tick.** Verified by hot-path-reviewer agent.

## New tables — 22 movers tables (one per timeframe)

### DDL pattern (illustrative for `movers_1s`)

```sql
CREATE TABLE IF NOT EXISTS movers_1s (
    ts                  TIMESTAMP,
    security_id         INT,
    segment             SYMBOL CAPACITY 16 NOCACHE,
    underlying_symbol   SYMBOL CAPACITY 1024 CACHE,
    instrument_type     SYMBOL CAPACITY 32 NOCACHE,
    ltp                 DOUBLE,
    prev_close          DOUBLE,
    change_pct          DOUBLE,
    change_abs          DOUBLE,
    volume              LONG,
    buy_qty             LONG,
    sell_qty            LONG,
    received_at         TIMESTAMP
) timestamp(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, security_id, segment);
```

### Table list (22 tables — same names as candle ladder but `movers_` prefix)

| # | Table | Cadence | Snapshot row count | Daily row volume | 90-day storage |
|---|---|---|---|---|---|
| 1 | `movers_1s` | 1 sec | 24,324 | ~86M rows/day | ~12GB |
| 2 | `movers_5s` | 5 sec | 24,324 | ~17M rows/day | ~2.4GB |
| 3 | `movers_10s` | 10 sec | 24,324 | ~8.7M rows/day | ~1.2GB |
| 4 | `movers_15s` | 15 sec | 24,324 | ~5.8M rows/day | ~800MB |
| 5 | `movers_30s` | 30 sec | 24,324 | ~2.9M rows/day | ~400MB |
| 6 | `movers_1m` | 1 min | 24,324 | ~1.4M rows/day | ~200MB |
| 7-20 | `movers_2m`..`movers_15m` | 2-15 min | 24,324 | ~700K..~95K rows/day each | ~10-100MB each |
| 21 | `movers_30m` | 30 min | 24,324 | ~48K rows/day | ~7MB |
| 22 | `movers_1h` | 1 hour | 24,324 | ~24K rows/day | ~3.5MB |
| **Total** | | | | **~125M rows/day** | **~17GB/90d** |

Fits the **100GB EBS hot tier** in `aws-budget.md` with margin. S3 lifecycle handles cold archive at 90d / 365d / Glacier-Deep tiers.

## DEDUP & uniqueness guarantee per I-P1-11

Every table uses `DEDUP UPSERT KEYS(ts, security_id, segment)`. The `segment` column is mandatory (per `security-id-uniqueness.md`). Two writes at the same `ts` with same `(security_id, segment)` collapse to one row — idempotent under restart, replay, or duplicate dispatch.

## New components — 6 modules (~1,800 LoC)

| # | Module | Path | Purpose |
|---|---|---|---|
| 1 | `MoversWriter22` | `crates/storage/src/movers_22tf_persistence.rs` | One ILP writer for all 22 tables. `append(timeframe_label, row)` + `flush()`. Schema-agnostic via shared row type. |
| 2 | `MoversSnapshotScheduler` | `crates/core/src/pipeline/movers_22tf_scheduler.rs` | 22 `tokio::spawn` tasks, each ticks at its cadence T, calls `MoversTracker::snapshot_full_universe()`, sends rows to writer. |
| 3 | `MoversTracker::snapshot_full_universe` | `crates/core/src/pipeline/top_movers.rs` (extend) | New method — return `Vec<MoverRow>` for ALL 24K instruments (not just top-N). Existing `record_tick` already maintains the per-instrument state. |
| 4 | `MoverRow` shared type | `crates/common/src/mover_types.rs` (new) | Single struct used across compute + persistence. `Copy` + zero-alloc serialization to ILP. |
| 5 | `movers_22tf_writer_state.rs` | `crates/core/src/pipeline/` | Global `OnceLock<MoversWriter22>` + `try_record_global` API mirroring `prev_close_writer.rs` pattern. |
| 6 | 9 new candle materialized views | `crates/storage/src/materialized_views.rs` | Add `candles_4m`, `candles_6m`, `candles_7m`, `candles_8m`, `candles_9m`, `candles_11m`, `candles_12m`, `candles_13m`, `candles_14m` (from `candles_1m`). |

## Hot-path latency guarantee — O(1)

Every per-tick step:
1. `MoversTracker::update_prev_close()` — `HashMap<(u32, u8), f32>::insert` — **O(1)**
2. `MoversTracker::record_tick()` — `HashMap<(u32, u8), SecurityState>::get_mut` — **O(1)**
3. `change_pct` arithmetic — pure float ops — **O(1)**

The 22 background snapshot tasks run **OFF the hot path** in dedicated `tokio::spawn` tasks. Each snap iterates the 24K HashMap once (O(N) but on a cold thread, every T seconds). ILP writes go through bounded `mpsc::channel(8192)` with drop-oldest fallback (same pattern as `prev_close_writer`).

## Zero-alloc proof on hot path

| Step | Allocation? | Why |
|---|---|---|
| `record_tick()` | NONE | `HashMap::get_mut` + struct field assignment |
| Snapshot scheduler tick | OFF hot path | Runs every T seconds in dedicated task |
| `try_send(MoverRow)` | NONE — `Copy` struct | `MoverRow` is bit-copyable (~64 bytes) |
| ILP serialization | Pre-allocated buffer per writer | Reuse `BufWriter<TcpStream>` — no per-row alloc |

Will add `dhat_movers_22tf_hot_path.rs` to mechanically pin `total_blocks == 0`.

## Restart-recovery / replay guarantee

| Failure | Recovery | Data loss? |
|---|---|---|
| App restart mid-day | `previous_close` table replays IDX_I prev_close into in-memory cache; NSE_EQ/NSE_FNO repopulate from first Full tick post-restart (typically <100ms per instrument) | NONE for IDX_I (table); ~100ms for stocks (acceptable — first tick arrives quickly) |
| QuestDB down | Rescue ring (existing) buffers ILP writes; on QuestDB recovery, drains in order; DEDUP UPSERT collapses duplicates | NONE (rescue ring + DEDUP) |
| Single snapshot task panics | Pool supervisor (similar to `respawn_dead_connections_loop`) re-spawns within 5s | At most 1 snapshot of 1 timeframe missed |
| Mid-snapshot tick gap | Next snapshot 1s later captures latest state | Stale by max 1s for `movers_1s`, max T seconds for higher TFs |

## Observability — Prometheus + Grafana + Telegram

### New metrics (10)

| Metric | Type | Purpose |
|---|---|---|
| `tv_movers_snapshot_rows_total{timeframe}` | counter | Rows written per timeframe |
| `tv_movers_snapshot_duration_ms{timeframe}` | histogram | Compute time per snapshot |
| `tv_movers_writer_errors_total{stage}` | counter | append/flush failures |
| `tv_movers_writer_dropped_total{reason}` | counter | full/closed/uninit drops |
| `tv_movers_universe_size` | gauge | Current tracked instrument count (target ~24,324) |
| `tv_movers_per_segment_count{segment}` | gauge | Per-segment instrument count (proves full-universe coverage) |
| `tv_movers_change_pct_p99{segment}` | gauge | P99 change % per segment (for sanity dashboard) |
| `tv_movers_snapshot_lag_seconds{timeframe}` | gauge | Lag between scheduled snap time and actual write |
| `tv_movers_questdb_writeback_lag_p99_ms{timeframe}` | gauge | Round-trip persistence lag |
| `tv_movers_table_row_count{timeframe}` | gauge | Row count in each movers_{T} table (sanity check) |

### New ErrorCodes (3) + runbooks

- **MOVERS-22TF-01:** Snapshot writer ILP failure (Severity::Medium)
- **MOVERS-22TF-02:** Snapshot scheduler task panicked (Severity::High)
- **MOVERS-22TF-03:** Universe size drift (target 24K ± 5%, fires if outside) (Severity::Medium)

Runbook: `.claude/rules/project/movers-22tf-error-codes.md`

### Telegram routing

Existing pattern — ErrorCode → Triage YAML → Telegram. Add 3 entries to `.claude/triage/error-rules.yaml`.

### Grafana panel — `movers-22tf.json`

| Panel | Query |
|---|---|
| Universe size by segment | `tv_movers_per_segment_count{segment=~".+"}` time series |
| Snapshot duration by TF | `tv_movers_snapshot_duration_ms` heatmap |
| Drop rate | `rate(tv_movers_writer_dropped_total[1m])` |
| Top 10 gainers @ 1m | `SELECT * FROM movers_1m WHERE ts = $__to AND segment = 'NSE_EQ' ORDER BY change_pct DESC LIMIT 10` |
| Top 10 gainers @ 5m | same with `movers_5m` |
| Per-segment writeback lag | `tv_movers_questdb_writeback_lag_p99_ms` |

## Ratchet tests — 12 new (per agent 4 proposal)

| # | Test | Pins |
|---|---|---|
| 1 | `test_22_movers_tables_ddl_complete` | All 22 tables exist with correct schema |
| 2 | `test_dedup_keys_include_segment_for_all_22_tables` | I-P1-11 across all tables |
| 3 | `test_timeframe_ladder_22_entries` | Constant `MOVERS_TIMEFRAMES.len() == 22` |
| 4 | `test_writer_isolation_per_timeframe` | One bad timeframe doesn't poison others |
| 5 | `test_universe_coverage_24k_instruments` | Every snapshot writes ≥24K rows |
| 6 | `test_upsert_idempotency_collapses_duplicates` | Two writes same `(ts, sec_id, seg)` → 1 row |
| 7 | `test_snapshot_cadence_matches_timeframe` | Each task fires at right interval ± 50ms |
| 8 | `test_prev_close_routing_per_segment_in_movers_compute` | NSE_EQ/NSE_FNO use tick.day_close, IDX_I uses cache |
| 9 | `test_27_candle_views_complete` | 18 + 9 new = 27 materialized views |
| 10 | `test_candle_view_bucket_alignment` | Hourly views use `OFFSET '00:15'`, others `'00:00'` |
| 11 | `test_e2e_movers_22tf_roundtrip` | Live tick → MoversTracker → snapshot → QuestDB → SELECT roundtrip |
| 12 | `test_ist_midnight_first_seen_set_reset` | First-Quote/Full per (id, seg) per day re-fires after IST 00:00 |

## DHAT zero-alloc tests (2 new)

- `dhat_movers_22tf_record_tick.rs` — 10K ticks → `total_blocks == 0`
- `dhat_movers_22tf_snapshot.rs` — 22 snapshots × 24K rows → `total_blocks == 0` (uses arena-allocated buffer)

## Criterion bench budgets (3 new in `quality/benchmark-budgets.toml`)

```toml
[budgets]
movers_record_tick = 50          # ns per tick
movers_snapshot_24k = 10_000     # μs (10ms) per full snapshot
movers_writer_enqueue = 200      # ns per row enqueue
```

## Implementation phases (5 commits)

| Phase | Commit | Files | LoC |
|---|---|---|---|
| 1 | `feat(storage): 22 movers_{T} tables + DDL + DEDUP keys` | `materialized_views.rs`, `movers_22tf_persistence.rs`, 1 schema test | ~400 |
| 2 | `feat(common): MoverRow shared type + 22-tf constants` | `mover_types.rs`, `constants.rs` | ~200 |
| 3 | `feat(core): MoversTracker::snapshot_full_universe + scheduler` | `top_movers.rs`, `movers_22tf_scheduler.rs`, 6 unit tests | ~600 |
| 4 | `feat(observability): metrics + ErrorCodes + Triage + Grafana + alerts` | `error_code.rs`, `events.rs`, `error-rules.yaml`, `movers-22tf.json`, `alerts.yml`, runbook | ~400 |
| 5 | `test(movers): 12 ratchets + 2 DHAT + 3 Criterion benches` | tests/, benches/, `benchmark-budgets.toml` | ~600 |
| **Total** | | | **~2,200 LoC** |

Fits stream-resilience.md rule B12 PR cap (≤3K LoC). Ships as ONE follow-up PR on top of #408.

## Open questions for Parthiban

| # | Question | My default |
|---|---|---|
| 1 | Approve the 9 extra candle views (4m, 6m, 7m, 8m, 9m, 11m, 12m, 13m, 14m)? | YES |
| 2 | Approve full-universe per-snapshot persistence (~125M rows/day, ~17GB/90d)? | YES |
| 3 | Approve unified `movers_{T}` table per timeframe (instead of split stock/option/index)? | YES |
| 4 | Approve `MoverRow` schema (12 columns above)? | YES |
| 5 | Add deprecation path for existing `top_movers`/`option_movers`/`preopen_movers` tables, or keep both? | KEEP both for now; deprecate after dashboard migrates |

## Verification gates (pre-merge)

- [ ] All 12 new ratchet tests pass
- [ ] DHAT zero-alloc on hot path
- [ ] Criterion bench-gate green
- [ ] Coverage 100% on new modules
- [ ] `make doctor` green post-boot
- [ ] Live boot — 22 tables populated within 1s of market open

## Risk register

| Risk | Mitigation |
|---|---|
| 24K snapshot every 1s = 24K writes/sec | Bounded mpsc(8192) + drop-oldest; QuestDB ILP sustained throughput >1M rows/sec verified in chaos tests |
| Snapshot scheduler clock drift | `tokio::time::interval` with `MissedTickBehavior::Skip` |
| Memory growth from 22 background tasks | All share single `Arc<MoversTracker>` — no per-task copy |
| Schema drift on adding new column later | Use `ALTER TABLE ADD COLUMN IF NOT EXISTS` pattern (existing schema self-heal) |
| Test count guard ratchet | Adding 12 + 2 + 3 = 17 new tests (only-increase ratchet OK) |
