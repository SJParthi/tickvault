# Wave 1 — Hot-path Remediation & Data Integrity (Items 0–4)

**Wave 1 PR target branch:** `claude/wave-1-hotpath-data-integrity` (from `claude/debug-session-error-6yqZV`)
**Estimated LoC:** ~3,200 (production + tests)
**Files touched:** ~25
**Merge precondition:** all 5 items VERIFIED + scoped tests + DHAT zero-alloc + bench-gate green

## Item 0 — Hot-path I/O + lock remediation (NEW, precondition)

**Source:** Background agent 2 (2026-04-27) verified 3 P0 stalls + bonus.

**REVISION 2026-04-27 (during Item 0.a kickoff):** Re-read of `tick_processor.rs:1515` showed the `RwLock::write()` is inside an explicit 5-second-gated cold-path block (`if last_snapshot_check.elapsed().as_secs() >= 5`) with comment "cold path, O(N log N)". The lock is held for the duration of a single `*guard = Some(snapshot)` assignment (nanoseconds). API readers almost never block. **Sub-item (c) downgraded — NOT a P0 stall, NOT in Wave 1 scope.** ArcSwap migration deferred to a future hardening pass if a real-world reader-blocking incident materializes.

| Stall | File:line | Current code | Fix | Wave 1? |
|---|---|---|---|---|
| (a) Sync `std::fs::write` + rename in PrevClose arm | `tick_processor.rs:1300-1314` | `std::fs::write(tmp, json).and_then(\|()\| std::fs::rename(tmp, path))` blocking on every code-6 packet | Move to dedicated writer task via `tokio::sync::mpsc::channel(64)` + `tokio::task::spawn_blocking`. Hot path enqueues `(path, bytes)`; writer drains. | ✅ Yes |
| (b) Sync `BufWriter<File>` on spill+DLQ | `tick_persistence.rs:567, 785, 594-630` | `writer.write_all(...)`, `writer.flush()` blocking when QuestDB down | Wrap spill writer with `tokio::sync::mpsc::channel(8192)` + dedicated `tokio::task::spawn_blocking` consumer. Bound queue, drop oldest with `tv_spill_dropped_total` counter on overflow. | ✅ Yes |
| ~~(c) `RwLock::write()` every 5s~~ | `tick_processor.rs:1515` | gated by 5s timer; lock held for ns | DEFERRED — not a P0 | ❌ Deferred |
| BONUS sync `fs::create_dir_all` per packet | `tick_processor.rs:1301` | redundant per-packet syscall | Hoist to one-shot at boot (`init_prev_close_cache_dir()`). | ✅ Yes |

### Tests (revised after sub-(c) deferral)

| Test | File | Asserts |
|---|---|---|
| `dhat_tick_path_zero_alloc_under_prev_close_burst` | `crates/core/tests/dhat_tick_path_zero_alloc.rs` | `total_blocks == 0` over 10K simulated PrevClose packets |
| `bench_prev_close_handler_le_100ns` | `crates/core/benches/prev_close_handler.rs` | Criterion p99 ≤ 100ns (enqueue only; actual fs::write happens in spawn_blocking task) |
| `bench_spill_write_enqueue_le_200ns` | `crates/storage/benches/spill_enqueue.rs` | Criterion p99 ≤ 200ns |
| `no_sync_fs_in_hot_path_guard` (source-scan) | `crates/core/tests/no_sync_fs_guard.rs` | Walk `crates/core/src/pipeline/` + hot files; fail on any `std::fs::*` without `// HOT-PATH-EXEMPT:` line |
| `chaos_questdb_down_spill_no_block` | `crates/storage/tests/chaos_spill_async.rs` | With QuestDB Docker paused, hot-tick latency stays ≤ 10μs (existing tick budget) |
| `test_prev_close_writer_task_drains_under_burst` | `crates/core/tests/prev_close_writer.rs` | 1000 enqueues → all written within 1s; no drops |
| `test_prev_close_cache_dir_init_idempotent` | same | `init_prev_close_cache_dir()` callable multiple times safely |

### Banned-pattern hook addition (category 7)

| Pattern | Allowed where | Rejected where |
|---|---|---|
| `std::fs::write`, `std::fs::create_dir`, `std::fs::create_dir_all`, `std::fs::rename`, `BufWriter::<File>::new` | tests, boot init, drain background tasks | `crates/core/src/pipeline/`, `crates/storage/src/tick_persistence.rs` hot fns, `crates/core/src/websocket/` parse path |
| Override comment | `// HOT-PATH-EXEMPT: <reason>` on the line above | — |

### 9-box

| Box | Value |
|---|---|
| Typed event | `HotPathSpillSaturated` (Severity::High), `HotPathPrevCloseEnqueueFailed` (Severity::Medium) |
| ErrorCode | `HOT-PATH-01` (sync I/O detected on hot path), `HOT-PATH-02` (spill drop) |
| tracing+code | yes, `code = ErrorCode::HOT_PATH_01.code_str()` |
| Prom counter | `tv_hotpath_spill_dropped_total`, `tv_hotpath_prev_close_writer_lag_seconds`, `tv_movers_swap_total` |
| Grafana panel | extend `operator-health.json` — Hot-path I/O panel |
| Alert rule | `tv_hotpath_spill_dropped_total` rate > 0 for 30s → CRITICAL |
| Call site | inline in `tick_processor.rs`, `tick_persistence.rs`, `top_movers.rs` |
| Triage YAML | new rules for `HOT-PATH-01/02` |
| Ratchet test | 7 above |

---

## Item 1 — Phase 2 ALWAYS-NOTIFY + EmitGuard (G7)

**Source:** existing plan + this plan G7.

| Aspect | Value |
|---|---|
| Files | `crates/app/src/main.rs` (Phase 2 dispatcher), `crates/core/src/notification/events.rs` |
| Change | After 09:13:00 IST trigger, exactly ONE of `Phase2Complete{added_count, depth_20_underlyings, depth_200_contracts}` or `Phase2Failed{reason, buffer_entries, skipped_no_price, skipped_no_expiry, sample_skipped: Vec<String>}` MUST emit unconditionally. Implement via `Phase2EmitGuard` Drop type — panics in `cfg(debug_assertions)`, emits ERROR `PHASE2-02` in release if dropped without firing. |
| Tests | `test_phase2_always_emits_complete_or_failed`, `test_phase2_failed_includes_diagnostic_fields`, `test_phase2_emit_guard_panics_in_debug_when_dropped`, `phase2_notify_guard.rs` source-scan ratchet (no early-return without emit call) |
| ErrorCode | `PHASE2-01` (Failed event fired), `PHASE2-02` (EmitGuard dropped without emission — should be unreachable in correct code) |
| Telegram | rides hardened dispatcher (Item 11) — coalesce key `("phase2", trading_date_ist)` |

---

## Item 2 — Stock + Index movers ALL ranks (no top-N)

| Aspect | Value |
|---|---|
| Files | `crates/app/src/main.rs` (movers task), `crates/storage/src/movers_persistence.rs`, `crates/core/src/pipeline/top_movers.rs` |
| Change | Remove `top: 20` truncation in `compute_snapshot`. Persist EVERY tracked instrument (~239 stocks + ~30 indices = ~269 rows) every 5s. Composite key `(security_id, exchange_segment, ts)`. Use `ArrayVec<[StockMover; 512]>` to avoid heap allocation; if exceeded, partition by exchange. |
| Schema | `CREATE TABLE IF NOT EXISTS stock_movers (ts TIMESTAMP, security_id LONG, segment SYMBOL, symbol SYMBOL, ltp DOUBLE, prev_close DOUBLE, change_pct DOUBLE, volume LONG, phase SYMBOL) timestamp(ts) PARTITION BY DAY DEDUP UPSERT KEYS(ts, security_id, segment);` |
| Tests | `test_compute_snapshot_returns_all_ranks_no_truncation`, `test_movers_persistence_writes_full_universe`, `test_movers_dedup_key_includes_segment`, `bench_compute_snapshot_239_stocks_le_1ms`, `dhat_movers_compute_zero_alloc` |
| ErrorCode | `MOVERS-01` (persist failed) |
| Prom | `tv_movers_persisted_total{kind="stock"|"index"}`, `tv_movers_compute_duration_seconds` histogram |

---

## Item 3 — Option movers (NEW — 22K contracts every 5s starting 09:15:00 IST)

| Aspect | Value |
|---|---|
| Files | NEW `crates/core/src/pipeline/option_movers.rs`, extend `crates/storage/src/movers_persistence.rs` (+`write_option_movers`) |
| Change | For every NSE_FNO option contract subscribed (~22,042 instruments), compute `change_pct = (ltp - prev_day_close) / prev_day_close * 100` where `prev_day_close` comes from Full packet `close` field (bytes 50-53, per Ticket #5525125). Persist ALL contracts; no top-N. Snapshot every 5s starting 09:15:00 IST. Partition compute by 3 underlying buckets (NIFTY/BANKNIFTY/SENSEX) if `compute_snapshot_22k_options` exceeds 5ms p99. |
| Schema | `CREATE TABLE IF NOT EXISTS option_movers (ts TIMESTAMP, security_id LONG, segment SYMBOL, underlying SYMBOL, expiry DATE, strike DOUBLE, option_type SYMBOL, ltp DOUBLE, prev_close DOUBLE, change_pct DOUBLE, oi LONG, volume LONG) timestamp(ts) PARTITION BY DAY DEDUP UPSERT KEYS(ts, security_id, segment);` |
| Tests | `test_option_movers_uses_full_packet_close_field`, `test_option_movers_persists_all_contracts`, `test_option_movers_starts_at_0915_sharp`, `bench_compute_snapshot_22k_options_le_5ms`, `dhat_option_movers_compute_zero_alloc`, `bench_option_mover_enrichment_lookup_le_50ns` |
| ErrorCode | `MOVERS-02` |
| Prom | `tv_option_movers_persisted_total`, `tv_option_movers_dropped_total{reason}`, `tv_option_movers_compute_duration_seconds` |
| Backpressure | If QuestDB ILP queue > 80% full → drop oldest snapshot with `tv_option_movers_dropped_total{reason="ilp_backpressure"}` counter |
| ILP flush | Pin `questdb.ilp.flush_interval_ms ≤ 1000` in `config/base.toml`; assertion at boot |

---

## Item 4 — `previous_close` UN-DEPRECATE + segment-routed persist + IST-midnight reset (G2, G3)

**Source:** Background agent 1 (2026-04-27) — parsers correct, persistence DEPRECATED with zero call sites, no first-Quote-per-day trigger, no ratchet tests.

### Sub-items

| Sub-item | Action |
|---|---|
| 4.1 | Un-deprecate `build_previous_close_row()` in `crates/storage/src/tick_persistence.rs:388` — remove `DEPRECATED` comment, reinstate schema DDL |
| 4.2 | Create NEW module `crates/storage/src/previous_close_persistence.rs` with `write_previous_close(security_id, segment, prev_close, source, trading_date_ist)` |
| 4.3 | Add `crates/core/src/pipeline/first_seen_set.rs` — papaya `HashSet<(u32, ExchangeSegment)>` with daily `reset_at_ist_midnight()` task |
| 4.4 | Wire 3 trigger paths in `tick_processor.rs`: code 6 (always for IDX_I), first Quote per (id, segment) (NSE_EQ), first Full per (id, segment) (NSE_FNO) |
| 4.5 | Add 3 ratchet tests + 1 dedup-key-includes-segment test + 1 IST-midnight reset test |

### Routing matrix

| Segment | Source | Byte offset | Trigger | First-seen-key |
|---------|--------|-------------|---------|----------------|
| IDX_I | PrevClose packet (code 6) | bytes 8-11 (f32 LE) | On every packet receipt (idempotent — DEDUP key handles repeats) | none — always write |
| NSE_EQ | Quote packet (code 4) `close` field | bytes 38-41 (f32 LE) | First Quote per `(security_id, NSE_EQ)` per IST trading day | `first_seen_set` |
| NSE_FNO | Full packet (code 8) `close` field | bytes 50-53 (f32 LE) | First Full per `(security_id, NSE_FNO)` per IST trading day | `first_seen_set` |

### Schema

```sql
CREATE TABLE IF NOT EXISTS previous_close (
    trading_date_ist DATE,
    security_id LONG,
    segment SYMBOL,
    prev_close DOUBLE,
    source SYMBOL,  -- 'CODE6' | 'QUOTE_CLOSE' | 'FULL_CLOSE'
    ts TIMESTAMP    -- when it was written (for audit)
) timestamp(ts) PARTITION BY MONTH
DEDUP UPSERT KEYS(trading_date_ist, security_id, segment);

ALTER TABLE previous_close ADD COLUMN IF NOT EXISTS source SYMBOL;
ALTER TABLE previous_close ADD COLUMN IF NOT EXISTS trading_date_ist DATE;
```

### Tests

| Test | Asserts |
|---|---|
| `test_prev_close_routing_idx_i_from_code6` | NIFTY=13 IDX_I → `source='CODE6'`, `prev_close` from bytes 8-11 |
| `test_prev_close_routing_nse_eq_from_quote_close_field` | RELIANCE NSE_EQ → `source='QUOTE_CLOSE'`, from bytes 38-41 |
| `test_prev_close_routing_nse_fno_from_full_close_field` | NIFTY-Jun26-25000-CE NSE_FNO → `source='FULL_CLOSE'`, from bytes 50-53 |
| `test_prev_close_dedup_key_includes_segment` | DEDUP key is `(trading_date_ist, security_id, segment)` — composite |
| `test_first_seen_set_resets_at_ist_midnight` | After IST 00:00, `first_seen_set` is empty; first Quote of new day re-triggers write |
| `test_trading_date_ist_not_utc` | Row written at IST 09:15 carries IST date, not UTC date |
| `test_previous_close_module_un_deprecated` | `build_previous_close_row` is callable, no `DEPRECATED` comment, has prod call sites |

### 9-box

| Box | Value |
|---|---|
| Typed event | `PrevCloseLoaded{security_id, segment, prev_close, source}` |
| ErrorCode | `PREVCLOSE-01` (write failed), `PREVCLOSE-02` (first-seen-set inconsistency) |
| Prom | `tv_prev_close_persisted_total{source}` (3 distinct sources by 09:30 IST), `tv_prev_close_first_seen_set_size` gauge |
| Grafana | extend `market-data.json` with prev-close coverage panel |
| Alert | any source < 1 by 10:00 IST → CRITICAL |
| Triage | rules for `PREVCLOSE-01/02` |
