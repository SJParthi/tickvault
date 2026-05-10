# Implementation Plan: Multi-TF In-Memory Aggregator + Direct Flush + Rehydrate-from-Ticks

**Status:** APPROVED (operator confirmed 2026-05-10, asked next session to execute)
**Date:** 2026-05-10
**Approved by:** Parthiban (operator)
**Wave:** 6 (post Wave-5 indices-only scope)
**Branch:** to be created from `main` AFTER PR #548 merges (e.g. `claude/wave-6-aggregator-direct-flush`)
**Scope size:** Multi-PR (4 sub-PRs minimum), 2-3 weeks calendar, ~3000-5000 LoC net

---

## MANDATORY HONEST ENVELOPE QUALIFIER (per CLAUDE.md + wave-4-shared-preamble.md)

> **100% inside the tested envelope, with ratcheted regression coverage:**
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤2,000,000-tick ring buffer (`TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`);
> ≤60s aggregator rehydration on cold boot (bench-gated);
> bench-gated O(1) hot path (DHAT zero-alloc + Criterion p99 ≤100ns);
> composite-key uniqueness `(security_id, exchange_segment)` per I-P1-11;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake.
> **Beyond envelope:** DLQ NDJSON catches every tick as recoverable text.
> **Outside envelope = disk-full or Dhan upstream outage; gap equals duration of that condition.**
>
> Literal "WebSocket never disconnects" / "QuestDB never fails" CANNOT be promised — TCP and OS physics make this impossible. The envelope above is the honest engineering guarantee.

---

## GOAL

Replace the `candles_1s` base table + 9 cascading materialized views with:
1. A **multi-timeframe in-memory aggregator** that builds 9 TFs (1m, 5m, 15m, 30m, 1h, 2h, 3h, 4h, 1d) directly from raw ticks
2. **Direct flush** of each sealed candle to per-TF QuestDB tables at seal time (no cascade)
3. **Rehydration on boot** by replaying ticks from the latest sealed timestamp per (security_id, exchange_segment, TF)
4. **Wave-5 % columns** stamped at seal time inside the aggregator (no cascade dependency)
5. **Wall-clock timer-driven seal** for sparse F&O contracts (NSE midnight + Muhurat 17:30-18:30 IST aware)
6. **Race-free** boot order: rehydrate → snapshot → start WS subscribe (live ticks buffered until rehydration completes)
7. **Fail-closed** boot: if rehydration fails partially, HALT with Telegram CRITICAL — do NOT start strategy/greeks/depth on incomplete state

---

## WHY (operator's principle, restated)

> "Ticks are source of truth. Trading reads from in-memory aggregator. DB is for replay/historical only. Save milliseconds for trading decisions."

Today this principle is partially met (trading reads in-memory 1s aggregator, no DB touches for decisions), but higher timeframes derive on the DB side via mat views. After this work, ALL timeframes derive in-memory, and DB is purely a parallel persistence sink.

---

## CRITICAL CONSTRAINTS DISCOVERED BY 3-AGENT REVIEW (2026-05-10)

The agent research session caught FALSE PREMISES that any implementation MUST resolve. Each constraint maps to a sub-PR.

| # | Severity | Finding | Resolution required in plan |
|---|---|---|---|
| 1 | CRITICAL | `CandleAggregator` is 1-second only TODAY. CLAUDE.md's "21 TFs" is wrong about this code. | Sub-PR #1 must build new multi-TF aggregator from scratch. NOT a refactor. |
| 2 | CRITICAL | Day-bucket seal must be wall-clock-timer driven (NSE midnight + Muhurat 17:30-18:30 IST). Currently tick-driven only. | Sub-PR #1 must include `BoundaryTimer` task per TF + `MuhuratCalendar`. |
| 3 | CRITICAL | Rehydration ↔ live WS race window: ticks fed twice OR skipped during boot. | Sub-PR #2 must implement `LiveTickBufferGate` that holds live WS ticks until rehydration completes. |
| 4 | CRITICAL | Partial rehydration → trading on incomplete data. | Sub-PR #2 must implement fail-closed boot halt + Telegram CRITICAL event. |
| 5 | CRITICAL | `LiveCandleWriter::append_candle` is SYNCHRONOUS today. 9 TFs sealing simultaneously = blocks tick processor. | Sub-PR #1 must add bounded `mpsc::channel(N)` per TF + dedicated drain task. |
| 6 | CRITICAL | Channel drop policy undefined. Default = silent block = SEV-1. | Sub-PR #1 must spec drop-newest + `tv_candle_writer_dropped_total{tf}` + edge-triggered Telegram. |
| 7 | HIGH | `[LiveCandle; 9]` contiguous array per security required for cache locality. 9 random HashMap lookups per tick = TLB misses. | Sub-PR #1 aggregator state must be `papaya::HashMap<(u32, u8), [LiveCandle; 9]>`. |
| 8 | HIGH | `last_cumulative_volume` tracker must stay shared across TFs (not duplicated 9×) for Item 28 session-reset. | Sub-PR #1 must enforce single shared `VolumeResetTracker` instance. |
| 9 | HIGH | DEDUP doesn't tell rehydration "was last bucket sealed-and-flushed." Always replay from `MAX(ts) - 1 bucket`. | Sub-PR #2 rehydration contract must document this. |
| 10 | HIGH | Multi-TF seal burst (1m+5m+15m+30m × 11K instruments = ~55K rows in one tick handler) → mpsc backpressure → tick loss. | Sub-PR #1 must spec mpsc capacity ≥ 65536 + spill-to-disk on overflow (mirror STORAGE-GAP-03 rescue-ring pattern). |
| 11 | HIGH | 7 production code paths break + ~50 tests need rewrite. Wave-5 % cascade boot order needs redesign. | Sub-PR #4 mechanical cleanup. |
| 12 | MEDIUM | Rehydration SELECT must STREAM (cursor), banned `.collect::<Vec>()`, MUST `ORDER BY ts ASC`, MUST `LIMIT N`. | Sub-PR #2 must use QuestDB PG-wire cursor or chunked `LIMIT/OFFSET`. |
| 13 | MEDIUM | TF-specific replay watermark: `MAX(ts) + 1 minute` for 1m, `+ 1 day` for 1d, NOT a single watermark. | Sub-PR #2 watermark fn signature: `fn replay_start(tf: Timeframe, latest_seal_ts: i64) -> i64`. |
| 14 | MEDIUM | App runs on host (no Docker `mem_limit`). OOM risk during rehydration. | Sub-PR #2 must add `MAX_REHYDRATION_TICKS` cap + benchmark RSS. |
| 15 | MEDIUM | DDL `execute_ddl_check_stale` allow-list still missing. | Sub-PR #1 must add compile-time allow-list (`&'static str` enum). |

---

## RECOMMENDED APPROACH: SHADOW-TABLE SPIKE (per agent recommendation)

**Do NOT big-bang.** The agents found 6 CRITICAL findings, all NEW code. Validate against production via shadow tables for 1 trading week before promotion.

| Phase | What runs in parallel | Verification | Duration |
|---|---|---|---|
| Phase 1 | Existing mat views (production) + new aggregator writes to `candles_1m_shadow`, `candles_5m_shadow`, ..., `candles_1d_shadow` | Cross-verify daily: `SELECT * FROM candles_1m EXCEPT SELECT * FROM candles_1m_shadow` → must be empty | 1 trading week |
| Phase 2 | Switch reads to shadow tables. Mat views still write but unread. | Monitor for 2 days | 2 trading days |
| Phase 3 | Drop mat views + `candles_1s` + `LiveCandleWriter`. Rename shadow tables to canonical. | Promotion | atomic |

---

## SUB-PR SEQUENCING (4 PRs, must merge in order)

### Sub-PR #1 — New multi-TF aggregator + bounded mpsc + boundary timer + Muhurat calendar
**Branch:** `claude/wave-6-pr1-multi-tf-aggregator`
**Files (NEW):**
- `crates/core/src/pipeline/multi_tf_aggregator.rs` — new aggregator with `[LiveCandle; 9]` per security
- `crates/core/src/pipeline/boundary_timer.rs` — wall-clock timer task per TF
- `crates/common/src/muhurat_calendar.rs` — Diwali Muhurat session detection
- `crates/storage/src/multi_tf_writer.rs` — bounded mpsc(65536) per TF + drain task + drop-newest + `tv_candle_writer_dropped_total{tf}`
- `crates/storage/src/aggregator_spill.rs` — STORAGE-GAP-03-mirror spill-to-disk on writer overflow

**Files (MODIFIED):**
- `crates/core/src/pipeline/mod.rs` (export new modules)
- `Cargo.toml` (no new deps expected)

**Files (NOT TOUCHED yet):** existing `candle_aggregator.rs`, `LiveCandleWriter`, mat views, `candles_1s` — runs in parallel.

**Deliverables:**
- Multi-TF aggregator builds 9 TFs in-memory from tick stream
- Wave-5 % columns computed at seal time per TF (`prev_day_cache_loader.rs::populate_prev_day_cache_at_boot` MUST run BEFORE aggregator spawn — boot order verified by ratchet)
- Wall-clock timer seals 1d at IST midnight + each hourly at the hour, regardless of tick presence
- Muhurat calendar correctly seals 1d bar that includes both regular (09:15-15:30) and Muhurat (17:30-18:30) sessions on Diwali
- Bounded mpsc + drop-newest policy + counter + Telegram on drop edge
- Spill-to-disk on writer overflow (NDJSON, mirror tick spill pattern)
- Writes go to `candles_*_shadow` tables (NOT yet replacing canonical)
- DEDUP UPSERT KEYS `(ts, security_id, exchange_segment)` on every shadow table

**Tests:**
- Unit: 9-TF seal at 09:30:00 boundary = exactly 4 candles per security (1m, 5m, 15m, 30m)
- Unit: 1d seal at IST midnight via timer (no tick)
- Unit: Muhurat session 17:30-18:30 included in same-day 1d bar
- Property: `aggregate(ticks[0..N]) == aggregate(replay(ticks[0..N]))` (purity)
- DHAT zero-alloc: aggregator hot path ≤4 alloc blocks/8KB across 10K calls
- Criterion: p99 per-tick aggregator update ≤100ns
- Loom: concurrent aggregator + writer drain task — no data race
- Integration: shadow-table writes match mat-view production output for fixture day

**Verification gates:** all 22 test categories per `testing.md` for affected crates.

### Sub-PR #2 — Rehydration boot path + race gate + fail-closed boot halt
**Branch:** `claude/wave-6-pr2-rehydration`
**Depends on:** Sub-PR #1 merged
**Files (NEW):**
- `crates/core/src/pipeline/rehydration.rs` — boot rehydration logic
- `crates/core/src/pipeline/live_tick_buffer_gate.rs` — buffer live WS ticks until rehydration completes
- `crates/storage/src/aggregator_rehydration_audit.rs` — audit table writer

**Files (MODIFIED):**
- `crates/app/src/main.rs` (boot sequence: rehydrate → snapshot → unblock buffer gate → WS subscribe)
- `crates/storage/src/materialized_views.rs` (add `aggregator_rehydration_audit` table DDL)

**Boot order (MUST be exact):**
1. QuestDB ready (`BOOT-01`)
2. Wave-5 prev_day_cache loaded (`PREVCLOSE-04`) — must precede aggregator
3. Multi-TF aggregator spawned, empty state
4. WS connection pool created BUT subscribe deferred (live tick buffer gate active)
5. **Rehydration:** for each TF, `SELECT max(ts) FROM candles_<tf>_shadow GROUP BY security_id, exchange_segment` → for each `(sid, segment, tf, max_ts)`, replay `SELECT * FROM ticks WHERE ts >= max_ts - 1_bucket ORDER BY ts ASC LIMIT MAX_REHYDRATION_TICKS` (streaming cursor)
6. Aggregator state validated (all expected sids covered, no NaN values)
7. **Audit row written** to `aggregator_rehydration_audit`
8. **Telegram:** `AggregatorRehydrationComplete` (Severity::Info) with counts
9. **Live tick buffer gate UNLOCKED** — buffered ticks drain through aggregator in ts order
10. WS subscribe fires
11. Normal operation resumes

**Fail-closed paths:**
- Rehydration takes > `REHYDRATION_DEADLINE_SECS` (default 300s = 5min) → HALT + `AggregatorRehydrationFailed` Telegram CRITICAL
- Rehydration finds inconsistent state (e.g., orphan candle without recent tick) → HALT
- `MAX_REHYDRATION_TICKS` cap hit per (sid, tf) → HALT (indicates ticks table corruption or impossibly large gap)
- Wave-5 prev_day_cache empty AND market hours → HALT (cannot stamp % columns correctly)

**New ErrorCodes (must add to `crates/common/src/error_code.rs` + runbook in `wave-6-error-codes.md`):**
- `REHYDRATE-01` Rehydration started (Info)
- `REHYDRATE-02` Rehydration completed (Info)
- `REHYDRATE-03` Rehydration deadline exceeded — HALT (Critical)
- `REHYDRATE-04` Rehydration partial state inconsistency — HALT (Critical)
- `REHYDRATE-05` `MAX_REHYDRATION_TICKS` cap hit — HALT (Critical)
- `BUFFER-GATE-01` Live tick buffer gate held live ticks (Info, debug)
- `BUFFER-GATE-02` Live tick buffer overflow — drop policy fired (High)

**Tests:**
- Property: post-rehydration aggregator state == pre-crash aggregator state (deterministic from ticks)
- Chaos: simulate crash at every minute boundary in a 6h trading session, verify rehydration recovers identical state
- Chaos: simulate QDB timeout mid-rehydration → assert HALT + Telegram fires
- Chaos: simulate ticks table corruption (missing rows) → assert HALT
- Integration: shadow-table cross-verify after rehydration matches pre-crash

### Sub-PR #3 — Cross-verify + Grafana migration to shadow tables
**Branch:** `claude/wave-6-pr3-readers-migrate`
**Depends on:** Sub-PR #2 merged + 1 trading week of shadow-vs-canonical zero-diff verification
**Files (MODIFIED):**
- `crates/core/src/historical/cross_verify.rs` (point queries at shadow tables OR keep mat-view comparison as a triple-check)
- `crates/core/src/pipeline/prev_oi_cache.rs` (verify `candles_1d` schema parity, no change if compatible)
- Grafana dashboards under `deploy/docker/grafana/dashboards/` (point candle panels at shadow tables)

**Verification:**
- Manual review of every Grafana panel's data source
- Daily zero-diff cross-verify report posted to Telegram for 2 trading days

### Sub-PR #4 — Promotion: drop candles_1s + LiveCandleWriter + 9 mat views, rename shadow tables to canonical
**Branch:** `claude/wave-6-pr4-promote`
**Depends on:** Sub-PR #3 merged + 2 trading days clean
**Files (DELETED):**
- `crates/storage/src/candle_persistence.rs::LiveCandleWriter` impl (lines 360-578)
- 9 mat-view DDL blocks in `crates/storage/src/materialized_views.rs`
- `crates/storage/src/materialized_views.rs::CANDLES_1S_TABLE_SQL`
- `crates/storage/src/materialized_views.rs::CANDLES_1S_DEDUP_SQL`
- `crates/common/src/constants.rs::QUESTDB_TABLE_CANDLES_1S`
- `crates/storage/src/partition_manager.rs:74` (remove `candles_1s` from `DAY_PARTITIONED_TABLES`)
- `crates/app/src/main.rs:1679-1684, 2832-2838, 7263-7284` (LiveCandleWriter spawn sites)
- `crates/storage/tests/grafana_query_tests.rs:252` (`schema_candles_1s_table`)
- `crates/core/tests/websocket_protocol_e2e.rs:743` (`assert_eq!(QUESTDB_TABLE_CANDLES_1S, "candles_1s")`)
- ~50 unit tests in `materialized_views.rs::tests` (`candles_1s_ddl_*`, `test_candles_1s_*`)

**Files (MODIFIED — RENAME):**
- `candles_1m_shadow` → `candles_1m`, etc. for all 9 TFs (atomic QuestDB rename if supported, else CREATE+COPY+SWAP+DROP)

**Banned-pattern hook update:**
- Add `.claude/hooks/banned-pattern-scanner.sh` category 7: rejects any new `candles_1s` reference, any new `LiveCandleWriter`, any `mat_view` for candle aggregation

**Verification:**
- `cargo check --workspace` clean
- `cargo test --workspace` all green (after deletion of dead tests)
- Banned-pattern scan green
- Grafana dashboards render
- 1 trading day post-promotion observation

---

## PER-ITEM 15-ROW MANDATORY GUARANTEE MATRIX (per per-wave-guarantee-matrix.md)

Every sub-PR (#1, #2, #3, #4) MUST carry this matrix before merge. Mechanical enforcement: `bash .claude/hooks/per-item-guarantee-check.sh` (exit 2 = block).

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` 100% min per crate | post-merge llvm-cov | item PR includes coverage delta |
| 100% audit coverage | `aggregator_rehydration_audit` + `multi_tf_writer_drop_audit` tables | `mcp__tickvault-logs__questdb_sql` | item adds/extends audit table |
| 100% testing coverage | 22 test categories per `testing.md` | `cargo test --workspace` green | item declares which 22 it covers |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory | all gates green |
| 100% performance | DHAT ≤4 alloc/8KB across 10K calls + Criterion p99 ≤100ns + bench-gate ≤5% regression | `cargo bench` + `scripts/bench-gate.sh` | DHAT test mandatory |
| 100% monitoring | 7-layer: Prom counter + gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table | `mcp__tickvault-logs__run_doctor` | 9-box completes 7 layers |
| 100% logging | `tracing` macros mandatory; ERROR → Telegram via Loki | hourly errors.jsonl rotation | every error path uses `error!` with `code` |
| 100% alerting | Prom rule in `alerts.yml` + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__list_active_alerts` | item adds alert |
| 100% security | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent | `cargo audit` post-deploy | item runs security-reviewer |
| 100% security hardening | DDL allow-list + pre-commit secret scan + `unused_must_use` lint | post-deploy IP verify | item declares attack-surface delta |
| 100% bug fixing | adversarial 3-agent review (proven pattern from this session) | pre-PR + post-impl agent pass | item runs all 3 agents |
| 100% scenario coverage | 9-box + chaos test for new failure mode | chaos suite | item declares scenarios |
| 100% functionality coverage | every pub fn has call site + test | pre-push gates 6+11 | item adds tests for every pub fn |
| 100% code review | adversarial 3-agent on diff before AND after impl | per-PR | item PR includes both passes |
| 100% extreme check | all of above + ratchet tests fail build on regression | every commit | item adds ratchet test |

## PER-ITEM 7-ROW RESILIENCE DEMAND MATRIX

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Bounded zero loss inside chaos envelope: ring 2M → spill NDJSON → DLQ | item must not introduce new tick-drop path |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | item must not break SubscribeRxGuard or pool watchdog |
| Never slow/locked/hanged | DHAT ≤4 alloc/8KB across 10K; Criterion p99 ≤100ns; tick-gap >30s Telegram; core_affinity Core 0 | item must not add hot-path allocation |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | item must not break self-heal |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% regression | item adds Criterion bench if hot path |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | item DEDUP key includes segment |
| Real-time proof | 7-layer telemetry + SLO-01/SLO-02 @ 10s + market-open self-test @ 09:16:30 IST | item ratchet pins all 7 layers |

---

## NEW PROMETHEUS METRICS

| Metric | Type | Labels | Purpose |
|---|---|---|---|
| `tv_multi_tf_aggregator_ticks_consumed_total` | counter | tf | Per-TF tick processing rate |
| `tv_multi_tf_aggregator_seals_total` | counter | tf, source (timer\|tick) | Per-TF seal events |
| `tv_multi_tf_writer_dropped_total` | counter | tf, reason (channel_full\|spill_full) | Drop policy fired |
| `tv_multi_tf_writer_spill_bytes_total` | counter | tf | Spill-to-disk volume |
| `tv_multi_tf_writer_pending_in_channel` | gauge | tf | Channel depth real-time |
| `tv_aggregator_rehydration_total` | counter | result (complete\|failed\|deadline) | Boot rehydration outcomes |
| `tv_aggregator_rehydration_seconds` | gauge | — | Last rehydration duration |
| `tv_aggregator_rehydration_ticks_replayed` | gauge | — | Last rehydration tick count |
| `tv_live_tick_buffer_gate_buffered` | gauge | — | Live ticks held in buffer (real-time) |
| `tv_live_tick_buffer_gate_drain_seconds` | gauge | — | Time to drain buffer post-rehydration |

---

## NEW TELEGRAM EVENTS

| Event | Severity | When |
|---|---|---|
| `MultiTfAggregatorReady` | Info | Boot, after rehydration completes |
| `AggregatorRehydrationComplete` | Info | Boot success with counts |
| `AggregatorRehydrationFailed` | Critical | Boot failure → HALT |
| `MultiTfWriterDrop` | High | Edge-triggered when drop counter increments after silence window |
| `MultiTfWriterSpill` | Medium | Spill-to-disk fired |
| `BoundaryTimerSealed` | Info (debug) | Wall-clock timer sealed bucket without tick (sparse F&O) |
| `MuhuratSessionDetected` | Info | Diwali Muhurat day boot, scheduler armed |
| `ShadowTableDiff` | High | Daily cross-verify diff between shadow and mat-view production output |

---

## NEW GRAFANA PANELS

- Per-TF seal rate (rate panel, 9 series)
- Aggregator rehydration duration (last 30 boots, single bar chart)
- Multi-TF writer pending-in-channel (gauge, 9 series, with red threshold at 80% capacity)
- Shadow vs canonical diff count over time (Phase 1-2 only)
- Boundary timer fires per TF (counter, 9 series)
- Live tick buffer gate buffered count (gauge with red threshold)

---

## NEW PROMETHEUS ALERTS

```yaml
- alert: tv-multi-tf-writer-dropping
  expr: rate(tv_multi_tf_writer_dropped_total[5m]) > 0
  for: 60s
  severity: high

- alert: tv-aggregator-rehydration-failed
  expr: increase(tv_aggregator_rehydration_total{result="failed"}[1d]) > 0
  for: 0s
  severity: critical

- alert: tv-aggregator-rehydration-slow
  expr: tv_aggregator_rehydration_seconds > 60
  for: 0s
  severity: high

- alert: tv-shadow-table-diff
  expr: tv_shadow_table_diff_total > 0
  for: 0s
  severity: high
```

All wired into `crates/storage/tests/resilience_sla_alert_guard.rs` ratchet.

---

## RATCHETS (will fail build on regression)

| Ratchet | Where | What it pins |
|---|---|---|
| `test_multi_tf_aggregator_purity` | property test | `aggregate(ticks) == aggregate(replay(ticks))` |
| `test_multi_tf_writer_bounded_channel_capacity` | unit | mpsc capacity = 65536 |
| `test_multi_tf_writer_drop_policy_is_drop_newest` | unit | drop policy + counter increment |
| `test_boundary_timer_seals_at_ist_midnight` | unit | wall-clock seal independent of ticks |
| `test_muhurat_calendar_includes_evening_session_in_1d` | unit | Diwali Muhurat handling |
| `test_rehydration_serial_before_ws_subscribe` | integration | boot order assertion |
| `test_rehydration_fail_closed_on_timeout` | chaos | HALT path verified |
| `test_live_tick_buffer_gate_holds_until_unlocked` | unit | gate semantics |
| `test_aggregator_dhat_zero_alloc` | dhat | hot-path zero-alloc invariant |
| `test_aggregator_criterion_p99_le_100ns` | bench | hot-path latency bound |
| `test_dedup_keys_include_segment_for_all_9_tfs` | meta-guard | I-P1-11 enforcement |
| `test_no_candles_1s_references_after_pr4` | banned-pattern | promotion completeness |
| `test_aggregator_rehydration_audit_table_exists` | DDL test | audit completeness |
| `test_wave5_pct_columns_stamped_at_seal` | unit | Wave-5 % column preservation |
| `test_prev_day_cache_loaded_before_aggregator_spawn` | boot order test | F2 ordering preserved |

---

## CHAOS TESTS (NEW)

| Chaos test | Scenario | Pass criteria |
|---|---|---|
| `chaos_aggregator_crash_every_minute_boundary` | Crash app at each minute boundary in a 6h trading session, restart, verify rehydration | All sealed candles match pre-crash; in-flight buckets correctly reconstructed |
| `chaos_qdb_timeout_mid_rehydration` | Pause QuestDB after 30s of rehydration | App HALTs with REHYDRATE-03, Telegram CRITICAL fires |
| `chaos_ticks_corruption_mid_replay` | Inject NULL into `ticks.ltp` mid-replay | App HALTs with REHYDRATE-04 |
| `chaos_multi_tf_seal_burst_at_30m_boundary` | Generate 11K securities × 1m+5m+15m+30m seal at 09:30:00 IST | No tick loss, no channel block, all 4×11K=44K rows persisted |
| `chaos_muhurat_day_aggregator` | Simulate Diwali Muhurat trading day | 1d bar correctly includes both regular and Muhurat sessions |
| `chaos_long_idle_aggregator_state_preserved` | Idle aggregator over 65h Fri→Mon weekend | State intact, day boundary timer fires correctly at each midnight |

---

## OPEN QUESTIONS FOR NEXT SESSION

| Question | Default if no operator answer |
|---|---|
| Skip 10s explicit checkpoint table (accept ≤60s rehydration)? | YES skip |
| Persist all 9 TFs (1m..1d) or fewer? | All 9 |
| Add `aggregator_rehydration_audit` table with 90-day hot + S3 cold retention? | YES |
| Multi-TF aggregator: build for ~11K (Wave-5 indices-only) or also stock-FNO ~25K? | Match `[subscription] scope` config — start ~11K |
| Boundary timer: per-second granularity or per-minute? | Per-minute (sufficient for 1m smallest TF) |
| Rehydration deadline: 5 minutes? | YES (300s) |
| `MAX_REHYDRATION_TICKS` cap per (sid, tf): 10M? | YES (configurable) |
| Atomic shadow→canonical rename via QuestDB? Confirm syntax. | Investigate at Phase 3 |

---

## NON-GOALS (explicitly out of scope)

- Trading-decision logic changes — UNCHANGED. Trading reads from in-memory aggregator (now 9-TF instead of 1s); zero DB touches for decisions.
- `ticks` table — UNCHANGED. Still raw IST epoch nanos source of truth.
- `historical_candles` table — UNCHANGED. Separate Dhan REST cold-fetch path.
- WebSocket/auth/order-update systems — UNCHANGED.
- Greeks pipeline — UNCHANGED. (No `candles_1s` reads.)
- Phase 2 dispatcher / depth rebalancer — UNCHANGED.

---

## NEXT-SESSION KICKOFF CHECKLIST

When the next Claude Code session starts on this plan, BEFORE any code:

1. Read this entire plan file end-to-end
2. Read `.claude/rules/project/wave-4-shared-preamble.md` (charter)
3. Read `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory matrix)
4. Read `.claude/rules/project/stream-resilience.md` (12 rules including B3 3-agent review)
5. Run `make doctor` (auto-loaded MCP) — confirm all systems green
6. Run `mcp__tickvault-logs__list_active_alerts` — must be empty
7. Verify PR #548 is merged to main and current branch is up to date with main
8. Create new branch `claude/wave-6-pr1-multi-tf-aggregator` from `main`
9. Spawn 3 fresh agents (hot-path-reviewer, security-reviewer, general-purpose hostile bug-hunt) for Sub-PR #1 specifically
10. Synthesize agent findings BEFORE writing code
11. Implement Sub-PR #1
12. Run all 22 test categories for changed crates per `testing-scope.md`
13. Open Sub-PR #1 as DRAFT against main
14. Wait for operator approval before starting Sub-PR #2

**CRITICAL: Do NOT big-bang. Sub-PR #1 ships shadow tables only. NO existing code is deleted until Sub-PR #4.**

---

## SCENARIO TABLE (every worst-case answered)

| # | Scenario | Detection | Recovery | Recovery time | Tick loss? |
|---|---|---|---|---|---|
| 1 | WS disconnect (TCP RST) | watchdog ≤5s | reconnect with SubscribeRxGuard | ≤10s | NO (rescue ring buffers) |
| 2 | App panic / OOM | OS detects | reboot → rehydrate | ≤60s + restart | NO (ticks durable) |
| 3 | App crash mid-aggregator-flush | next boot finds incomplete bucket | replay from `MAX(ts) - 1 bucket`, DEDUP UPSERT handles re-flush | ≤60s | NO |
| 4 | QuestDB crash | rescue ring fills | spill to NDJSON; on QDB return, drain | ≤60s envelope | NO (inside envelope) |
| 5 | QuestDB OOM | same as crash | same | ≤60s | NO |
| 6 | Network partition app↔QDB | rescue ring fills | drain on heal | bounded by 2M ring | NO |
| 7 | Long idle weekend (65h) | sleep-until-open Wave-2 WS-GAP-04 | aggregator state preserved across idle | 0s (state intact) | NO |
| 8 | Holiday weekend (92h) | same | same | 0s | NO |
| 9 | Token expires mid-session | DH-901 detected | force-renew Wave-2 AUTH-GAP-03 | ≤15s | NO |
| 10 | Multi-TF seal burst at 09:30:00 | bounded mpsc | 65536 capacity absorbs ~55K row burst | <1s | NO (capacity headroom) |
| 11 | mpsc overflow (>65536 in flight) | spill-to-disk fires | drain to candles_*_spill NDJSON | bounded | NO (recoverable) |
| 12 | Sparse F&O contract no ticks for hours | boundary timer fires at IST midnight | wall-clock seal | 0s | NO (no ticks to lose) |
| 13 | Diwali Muhurat session | Muhurat calendar detects | 1d bar includes both regular + Muhurat ticks | 0s | NO |
| 14 | Cosmic-ray bit-flip in aggregator | volume monotonicity guard Wave-5 VOLUME-MONO-01 | Telegram alert + force re-rehydration | ≤60s | NO (ticks durable) |
| 15 | Disk full on app host | BOOT-02 alarm | App halts. Operator clears. Ticks resumed from Dhan from disconnect-time. | bounded by disk-full duration | YES (gap = disk-full duration) — outside envelope |
| 16 | Dhan upstream outage | tick-gap detector | nothing we can do — Dhan owns the source | bounded by Dhan recovery | YES (gap = Dhan outage) — outside envelope |
| 17 | Rehydration deadline > 5min | REHYDRATE-03 alarm | HALT, Telegram CRITICAL, operator inspects | indefinite | NO (fail-closed prevents corruption) |
| 18 | Rehydration finds inconsistent state | REHYDRATE-04 | HALT, Telegram CRITICAL | indefinite | NO |
| 19 | Wave-5 prev_day_cache empty at boot during market hours | PREVCLOSE-04 (existing) | HALT (cannot stamp % columns correctly) | indefinite | NO |
| 20 | Live tick buffer overflow during rehydration | BUFFER-GATE-02 | drop-newest with counter, Telegram | bounded | YES (drop-newest) — operator alarm |

---

## FINAL HONEST ASSESSMENT

| Aspect | Verdict |
|---|---|
| Operator's "ticks → memory → DB for replay" principle | Already met today. This plan extends it to all 9 TFs. |
| Real benefit | Fewer DB-side moving parts (9 mat views → 9 plain tables); no OFFSET/cascade gotchas; cleaner add-new-TF path. |
| Real cost | 2-3 weeks engineering. Net app-memory +38 MB. New code = new bug surface (mitigated by shadow phase). |
| Risk if rushed (big-bang) | High — 6 CRITICAL agent findings, all NEW code, untested on live data. |
| Risk if shadow-phased | Low — 1 trading week parallel run + cross-verify catches divergence. |
| Trading decision impact | ZERO (already in-memory off ticks; no DB reads for decisions). |

**This plan is APPROVED in scope. Next session executes Sub-PR #1 first.**
