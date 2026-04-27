# Wave 2 — Resilience (Items 5–9)

**Wave 2 PR target branch:** `claude/wave-2-resilience` (from main after Wave 1 merge)
**Estimated LoC:** ~3,800
**Files touched:** ~30
**Merge precondition:** Wave 1 merged + all 5 items VERIFIED + chaos suite green

## Item 5 — Main-feed WebSocket NEVER GIVES UP + token-aware wake (G1)

**Source:** existing plan + G1 verified by agent 3 (2026-04-27).

| Sub-item | Action |
|---|---|
| 5.1 | Replace post-close `return false` at `crates/core/src/websocket/connection.rs:1285` with infinite retry loop calling `sleep_until_next_market_open_ist()` |
| 5.2 | Implement `sleep_until_next_market_open_ist()` using `TradingCalendar` — skip weekends + NSE holidays |
| 5.3 | Add pool supervisor `respawn_dead_connections_loop` in `connection_pool.rs` — re-spawn any task that exits |
| 5.4 | **Token-aware wake (G1):** before reconnect after sleep, call `TokenManager::force_renewal_if_stale(threshold_secs=14400)` (4h headroom). Add `force_renewal()`, `next_renewal_at()` public API to `TokenManager`. |
| 5.5 | New typed events: `WebSocketSleepEntered{feed, sleep_until_ist}` (Severity::Low), `WebSocketSleepResumed{feed, slept_for_secs}` (Severity::Info), `WebSocketTokenForceRenewedOnWake` (Severity::Low) |

### Sleep semantics

| Trigger | Action |
|---|---|
| 3 consecutive reconnect failures outside `[09:00, 15:30) IST` | Close socket; sleep until next market open |
| Inside market hours | Infinite immediate retry (Dhan pings every 10s) |
| Holiday or weekend after sleep | Wake at 09:00 next trading day (skip non-trading days via `TradingCalendar`) |
| Token < 4h to expiry on wake | Force-renew BEFORE reconnect attempt |

### Tests

| Test | File |
|---|---|
| `test_main_feed_post_close_sleeps_until_next_open` | `crates/core/tests/ws_sleep_resilience.rs` |
| `test_ws_sleep_skips_weekend_to_monday_0900` | same |
| `test_ws_sleep_skips_holiday_via_trading_calendar` | same |
| `test_pool_supervisor_respawns_dead_connection_within_5s` | `crates/core/tests/pool_supervisor.rs` |
| `test_token_force_renewal_on_wake_when_stale` | `crates/core/tests/token_wake_renewal.rs` |
| `test_70h_sleep_then_connect_succeeds` | `crates/core/tests/auth_long_sleep.rs` |
| `test_no_reconnect_exhaustion_path_remains` | source-scan ratchet |

### 9-box

| Box | Value |
|---|---|
| ErrorCode | `WS-GAP-04` (sleep entered), `WS-GAP-05` (supervisor respawn), `AUTH-GAP-03` (token force-renewed on wake) |
| Prom | `tv_ws_post_close_sleep_total{feed}`, `tv_ws_pool_respawn_total{feed}`, `tv_token_force_renewal_total{trigger="ws_wake"}` |
| Alert | `tv_ws_pool_respawn_total` rate > 1/min for 5min → HIGH (cascading failure) |

---

## Item 6 — Depth-20 + Depth-200 + Order-Update WS NEVER GIVE UP

| Aspect | Value |
|---|---|
| Files | `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/websocket/order_update.rs` |
| Change | Same supervisor pattern as Item 5. 3 independent sleep clocks per WS pool. Depth's 60-attempt cap (`websocket-enforcement.md` rule 14) becomes "60 in-market attempts THEN sleep until 09:00". Order-Update activity watchdog STAYS 14400s (4h) per `live-market-feed-subscription.md` 2026-04-24 §7 — do not regress to 1800s. |
| Tests | 3 × `test_<feed>_post_close_sleeps_until_next_open`, 3 × `test_<feed>_supervisor_respawns_dead_task`, 1 × `test_order_update_activity_watchdog_remains_14400s` |
| ErrorCode | reuse `WS-GAP-04/05` with `feed` label dimension |

---

## Item 7 — FAST BOOT race fix (60s deadline, escalating logs) (G14)

**Source:** existing plan + G14 (auto-up Docker can take 30–60s on cold pull).

| Aspect | Value |
|---|---|
| Files | `crates/app/src/main.rs` (boot sequence), `crates/storage/src/tick_persistence.rs` |
| Change | Tick processor calls `wait_for_questdb_ready(deadline_secs=60)` before consuming SPSC. Escalating logs at +5s (DEBUG), +10s (INFO), +20s (WARN), +30s (ERROR `BOOT-01`), +60s (CRITICAL `BOOT-02` + HALT). During wait, ticks accumulate in rescue ring silently for first 30s, then escalate. |
| Tests | `test_tick_processor_waits_for_questdb_ready`, `test_rescue_ring_silent_during_boot_window_30s`, `test_critical_at_30s_warn_at_20s`, `test_halt_at_60s` |
| ErrorCode | `BOOT-01` (slow boot, > 30s), `BOOT-02` (boot deadline exceeded — HALT) |
| Prom | `tv_boot_questdb_ready_seconds` histogram, `tv_boot_rescue_ring_size` gauge |
| Telegram | `BootFastQuestDBSlow` (Severity::High at +30s), `BootDeadlineExceeded` (Severity::Critical at +60s) |

---

## Item 8 — Tick-gap RCA — composite-key papaya + 60s coalesce (G4)

**Source:** existing plan Item 10 + G4 (coalescing).

| Aspect | Value |
|---|---|
| Files | `crates/core/src/pipeline/tick_processor.rs`, `crates/core/src/pipeline/tick_gap_detector.rs` (NEW), `crates/core/src/instrument/subscription_planner.rs` |
| Change | `papaya::HashMap<(u32, ExchangeSegment), Instant>` keyed on COMPOSITE per I-P1-11. Threshold `tick_gap_threshold_seconds = 30` configurable in `config/base.toml`. Per-instrument detection at 30s; **Telegram coalescing — emit 1 summary every 60s with count + top-10 sample**, never one-per-instrument (G4). Daily reset at 15:35 IST (G19). |
| Tests | `test_tick_gap_detected_after_30s_silence_per_instrument`, `test_tick_gap_telegram_coalesced_per_60s`, `test_tick_gap_uses_composite_key_not_security_id_alone`, `test_subscribe_rx_guard_survives_overnight_16h_synthetic`, `test_tick_gap_map_resets_at_1535_ist`, `bench_tick_gap_lookup_le_50ns` |
| ErrorCode | `WS-GAP-06` (TickGapDetected — coalesced summary) |
| Prom | `tv_tick_gap_total{security_id, segment}`, `tv_tick_gap_seconds_p99`, `tv_tick_gap_map_entries` gauge |
| Telegram event | `TickGapsSummary{count, top_10_samples: Vec<(symbol, gap_secs)>, window_secs: 60}` (Severity::Medium) |

---

## Item 9 — 6 Audit Tables with Retention Policy

**Source:** user's text (5 audit tables + previous_close re-enabled = 6).

### Table set

| # | Table | Composite key | DEDUP UPSERT KEYS | Partition | Retention |
|---|-------|--------------|-------------------|-----------|-----------|
| 1 | `phase2_audit` | `(trading_date_ist, ts)` | same | DAY | 90d hot → S3 IT → Glacier |
| 2 | `depth_rebalance_audit` | `(underlying_symbol, ts)` | same | DAY | 90d hot → S3 IT → Glacier |
| 3 | `ws_reconnect_audit` | `(connection_id, ts)` | same | DAY | 90d hot → S3 IT → Glacier |
| 4 | `boot_audit` | `(boot_id, step)` (ULID for boot_id) | same | DAY | 30d hot |
| 5 | `selftest_audit` | `(trading_date_ist, check_name)` | same | DAY | 90d hot → S3 IT → Glacier |
| 6 | `order_audit` | `(order_id, ts, leg)` | same | DAY | 5y SEBI requirement (per `aws-budget.md`) |

### Schema example — depth_rebalance_audit

```sql
CREATE TABLE IF NOT EXISTS depth_rebalance_audit (
    ts TIMESTAMP,
    underlying_symbol SYMBOL,
    old_atm_strike DOUBLE,
    new_atm_strike DOUBLE,
    old_security_ids LONG[],
    new_security_ids LONG[],
    spot_at_swap DOUBLE,
    swap_levels SYMBOL,  -- 'Depth-20' | 'Depth-20+200'
    outcome SYMBOL       -- 'Success' | 'Failed'
) timestamp(ts) PARTITION BY DAY
DEDUP UPSERT KEYS(underlying_symbol, ts);
```

### Files

| File | Purpose |
|---|---|
| `crates/storage/src/phase2_audit_persistence.rs` | NEW |
| `crates/storage/src/depth_rebalance_audit_persistence.rs` | NEW |
| `crates/storage/src/ws_reconnect_audit_persistence.rs` | NEW |
| `crates/storage/src/boot_audit_persistence.rs` | NEW |
| `crates/storage/src/selftest_audit_persistence.rs` | NEW |
| `crates/storage/src/order_audit_persistence.rs` | NEW |

### Self-heal at boot

Each module emits idempotent `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ADD COLUMN IF NOT EXISTS` for every column added in this PR set, per `observability-architecture.md` schema self-heal rule.

### S3 lifecycle (per C13)

`deploy/aws/s3-lifecycle.json`:
- Day 0–90 → S3 Standard
- Day 90–365 → S3 Intelligent-Tiering
- Day 365+ → Glacier Deep Archive (90-day minimum honored — partition manager NEVER recreates archived partitions, idempotency-key `(table, partition_date)` in `s3_archive_log`)

### Tests

| Test | Asserts |
|---|---|
| 6 × `test_<table>_dedup_key_includes_segment` (where applicable) | DEDUP key composite |
| 6 × `test_<table>_idempotent_alter_add_column_if_not_exists` | Schema self-heal works on existing data |
| `test_partition_manager_never_recreates_archived_partition` | Glacier 90-day minimum honored |
| `dedup_segment_meta_guard.rs` extension | Source-scan: every new persistence module's DEDUP key includes segment if security_id is part of key |
| `test_s3_archive_log_idempotency_key_unique` | Idempotency-key uniqueness assertion |

### 9-box

| Box | Value |
|---|---|
| ErrorCode | `AUDIT-01..06` per table; `STORAGE-GAP-03` (audit write failure), `STORAGE-GAP-04` (S3 archive failure) |
| Prom | `tv_audit_persisted_total{table}`, `tv_audit_write_failures_total{table}` |
| Grafana | New `audit-trails.json` dashboard with 6 panels |
| Alert | any audit write failure rate > 0.1/sec for 30s → HIGH |
