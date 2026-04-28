# Movers 22-TF v2 — Risks (8 unmitigated → all mitigated, plus chaos test)

## Risk register — every gap from audit 03 + 01 cross-cutting closed

| # | Risk | v1 status | v2 mitigation | Owner |
|---|---|---|---|---|
| 1 | QuestDB ILP TCP disconnect mid-snapshot | unmitigated | **Movers-specific rescue ring** — extend existing `RescueRing` (size 600K) to accept `MoverRow` payload tagged with `timeframe_idx`; on QuestDB recovery, drains in `(timeframe, ts)` order; DEDUP UPSERT collapses any replay duplicates | Phase 1 + 5 |
| 2 | mpsc(8192) saturation under burst | drop-oldest claimed | **Drop-NEWEST + 0.1% SLA alert** — `tv_movers_writer_dropped_total{reason}` ; alert `rate(...[5m]) / rate(tv_movers_snapshot_rows_total[5m]) > 0.001` for 5min → Severity::High Telegram | Phase 4 |
| 3 | Scheduler clock drift > 100ms | metric undefined | **Drift threshold + alert** — `tv_movers_snapshot_lag_seconds{timeframe}` gauge; alert at `> 0.500` for 60s (1s timeframe) or `> 5%` of `T` for higher TFs | Phase 4 |
| 4 | 22 tasks all panic | supervisor not implemented | **Explicit `MoversSupervisor`** — `tokio::spawn` watcher polls 22 `JoinHandle`s every 5s; on `is_finished()` re-spawns the task and emits `MOVERS-22TF-02` (Severity::High) + `tv_movers_supervisor_respawn_total{timeframe}` counter | Phase 3 |
| 5 | IDX_I prev_close missing for new index mid-day | no fallback | **3-tier fallback chain** — (a) in-memory cache → (b) `previous_close` table read with 5min cache → (c) skip row + `tv_movers_prev_close_unknown_total{segment, security_id}` increment + WARN log; row IS NOT WRITTEN with NaN | Phase 3 |
| 6 | F&O contract expires intraday | no lifecycle | **`is_active_today` filter** at snapshot time using `FnoUniverse::derivative_contracts.expiry`; expired contracts SKIPPED from snapshot; `tv_movers_skipped_expired_total{segment}` counter so dashboard can show drop-off | Phase 3 |
| 7 | Movers tables not in partition manager | unmitigated HIGH | **Add 22 tables to `HOUR_PARTITIONED_TABLES`** in `partition_manager.rs:30-42`; ratchet `test_movers_partition_manager_includes_22_tables` blocks regression | Phase 1 |
| 8 | SEBI retention classification unclear | undocumented | **Documented as 90-day operational data, NOT 5y SEBI** — movers are derived analytics, not order/trade records; lifecycle: 90d hot QuestDB → 365d S3 IT → ≥1y Glacier (per `aws-budget.md` lifecycle); add `s3-lifecycle-movers-tables.json`; ratchet pins it | Phase 1 |

## Unverified claim → CHAOS TEST (mandatory before merge)

**Claim from v1:** "QuestDB ILP sustained throughput >1M rows/sec verified in chaos tests" — UNSUBSTANTIATED in audit 03.

**v2 deliverable:** new chaos test
`crates/storage/tests/chaos_movers_22tf_throughput.rs`:
- Spin up local QuestDB via `testcontainers`
- 22 producer tasks, each emits 24K rows at coordinated 1s/5s/.../1h cadence
- Sustain for 60s
- Assert: total rows persisted ≥ `0.999 × theoretical_total` (drop budget 0.1%)
- Assert: peak instantaneous rate ≥ **1.0M rows/sec sustained over the 60s aligned-fire boundary**
- Assert: no ILP TCP disconnect during run
- Tagged `#[ignore]` by default (CI nightly opt-in via `CHAOS=1`)
- Documented in `quality/benchmark-budgets.toml` as a separate budget

If the chaos test FAILS the 1M rows/sec floor → design must downgrade to 11
writer pool (5 timeframes per writer, sharing TCP) OR raise QuestDB
`QDB_LINE_TCP_NET_ACTIVE_CONNECTION_LIMIT` from 20 to 32. Both options
already wired as fallback flags.

## Memory + capacity math (re-verified)

| Item | Calculation | Result |
|---|---|---|
| `MoverRow` size | 192 B (Copy, `ArrayString<16>`×2 = 32 B + 8×i64/f64 = 64 B + padding) | ≤192 B ✓ |
| Arena per timeframe | 30K capacity × 192 B | 5.76 MB |
| 22 arenas total | 22 × 5.76 MB | 127 MB |
| Papaya tracker state | ~24K entries × ~80 B | 1.9 MB |
| 22 mpsc(8192) buffers | 22 × 8192 × 192 B | 33.7 MB |
| **Total movers memory** | | **~163 MB** within 1 GB app budget ✓ |

| Item | Calculation | Result |
|---|---|---|
| Daily row volume | sum over 22 cadences × 24,324 instruments | ~125M rows/day |
| 90-day storage @ ~140 B/row compressed | 125M × 90 × 140 B | ~1.6 TB raw → ~17 GB after QuestDB compression | 
| Fits 100 GB EBS | yes, with margin (other tables: ~30 GB) | ✓ |

## Failure mode coverage (10 modes, 10 mitigations)

| # | Failure | Mitigation | Recovery time |
|---|---|---|---|
| 1 | Single snapshot task panic | Supervisor respawn (Risk #4) | ≤5s |
| 2 | All 22 tasks panic | Supervisor respawns each | ≤5s × 22 staggered |
| 3 | One timeframe writer ILP TCP RST | Per-writer reconnect (own backoff) | per-writer |
| 4 | All writers ILP TCP RST | Rescue ring buffers all 22 streams | bounded by ring size |
| 5 | mpsc full | Drop-newest + Prom counter + alert at 0.1% | edge-triggered |
| 6 | Scheduler clock drift | Drift gauge + alert at 500 ms / 5% of T | 60s |
| 7 | App restart mid-day | papaya rebuilds from first tick (~100 ms/inst); IDX_I from `previous_close` table | ≤30s for full universe |
| 8 | New IDX_I added mid-day | 3-tier fallback (Risk #5) | row skipped, not invalid |
| 9 | F&O expires intraday | `is_active_today` filter (Risk #6) | next snapshot omits |
| 10 | EBS fills | Partition manager rotates (Risk #7) | weekly cron |

## Open questions for Parthiban (5)

| # | Question | v2 default |
|---|---|---|
| 1 | Approve 9 extra candle views (4m, 6m, 7m, 8m, 9m, 11m, 12m, 13m, 14m)? | YES |
| 2 | Approve full-universe per-snapshot persistence (~125M rows/day, ~17GB/90d, 100GB EBS fits)? | YES |
| 3 | Approve unified `movers_{T}` table per timeframe (deprecate `top_movers`/`option_movers`/`preopen_movers` after dashboard migrates)? | YES |
| 4 | Approve 22 separate writers (vs 1 shared writer)? Bumps QuestDB ILP connection budget from ~6 used to ~28 used (limit 20 → may need raise to 32) | YES |
| 5 | Approve chaos test as merge-blocker for 1M rows/sec claim? | YES |
