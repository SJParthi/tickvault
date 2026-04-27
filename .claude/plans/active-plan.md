# Implementation Plan: WebSocket Startup Hardening + Movers + Phase 2 Notify

**Status:** DRAFT
**Date:** 2026-04-27
**Approved by:** pending
**Branch:** `claude/fix-websocket-startup-p0x1O`
**Triggering incident:** Overnight 2026-04-26 → 2026-04-27 — app started 22:21 IST Sun, all 5 main-feed connections were dead at 09:00 IST Mon market open. Operator had to manually restart the binary at 09:03:51. Depth, stock_movers, option_movers, previous_close all broken simultaneously.

## Three Principles (every item must pass all three)

| # | Principle | Mechanism in this plan |
|---|-----------|------------------------|
| 1 | **O(1) latency on hot path** | papaya HashMap for spot lookups; ArrayVec for batch buffers; rtrb SPSC for movers; no allocation in tick path |
| 2 | **Uniqueness guarantee** | `(security_id, exchange_segment)` composite key everywhere (I-P1-11) — movers, prev_close, dedup sets |
| 3 | **Deduplication** | QuestDB `DEDUP UPSERT KEYS(...)` includes segment in every new table; idempotent ALTER ADD COLUMN IF NOT EXISTS |

## Authoritative Facts (do NOT relitigate)

| Fact | Source |
|------|--------|
| PrevClose packet (code 6) — **IDX_I ONLY** | `.claude/rules/dhan/live-market-feed.md` rule 7; Dhan Ticket #5525125 (2026-04-10) |
| For NSE_EQ → read `close` field from Quote packet bytes 38-41 (f32 LE) | Same rule 8 |
| For NSE_FNO → read `close` field from Full packet bytes 50-53 (f32 LE) | Same rule 10 |
| Movers = **ALL** equities + ALL F&O + ALL indices (no top-N truncation) | Operator directive 2026-04-27 |
| Pre-open buffer window 09:00..=09:12 IST | `.claude/rules/project/depth-subscription.md` 2026-04-24 §1 |
| Phase 2 trigger 09:13:00 IST | Same §4 |
| Streaming heartbeat 09:15:30 IST | Same §8 |
| Depth-200 root path `/?token=...` | `.claude/rules/dhan/full-market-depth.md` rule 2 |

## The 10 Plan Items

Every item must complete the **9-box observability checklist** (Typed event / ErrorCode / tracing+code / Prometheus counter / Grafana panel / Alert rule / Production call site / Triage YAML / Ratchet test) plus runbook.

### P0 — DATA-INTEGRITY (broken today, blocks operator visibility)

- [ ] **Item 1: Phase 2 ALWAYS-NOTIFY guarantee**
  - Files: `crates/app/src/main.rs` (Phase 2 dispatcher), `crates/core/src/notification/events.rs`
  - Change: After 09:13:00 IST trigger, exactly ONE of `Phase2Complete{added_count, depth_20_underlyings, depth_200_contracts}` or `Phase2Failed{reason, buffer_entries, skipped_no_price, skipped_no_expiry, sample_skipped}` MUST emit unconditionally. No silent path.
  - Tests: `test_phase2_always_emits_complete_or_failed`, `test_phase2_failed_includes_diagnostic_fields`
  - Ratchet: `crates/app/tests/phase2_notify_guard.rs` source-scan that the dispatcher has no early-return without an event emit.

- [ ] **Item 2: Stock + Index movers PERSISTENCE (no top-N limit, ALL ranks)**
  - Files: `crates/app/src/main.rs` (movers task), `crates/storage/src/movers_persistence.rs`, `crates/core/src/pipeline/top_movers.rs`
  - Change: Remove `top: 20` truncation in `compute_snapshot`. Persist EVERY tracked instrument (~239 stocks + ~30 indices = full universe) via `write_to_questdb` after each compute. Composite key `(security_id, exchange_segment, ts)`.
  - Tests: `test_compute_snapshot_returns_all_ranks_no_truncation`, `test_movers_persistence_writes_full_universe`, `test_movers_dedup_key_includes_segment`
  - Schema: `stock_movers` table with `DEDUP UPSERT KEYS(ts, security_id, segment)`.

- [ ] **Item 3: Options movers (NEW — fires at 09:15:00 IST sharp, ALL ranks)**
  - Files: NEW `crates/core/src/pipeline/option_movers.rs`, `crates/storage/src/movers_persistence.rs` (+ `write_option_movers`)
  - Change: For every NSE_FNO option contract (~22,042 instruments), compute `change_pct = (ltp - prev_day_close) / prev_day_close * 100` where `prev_day_close` comes from Full packet `close` field (bytes 50-53, per Ticket #5525125). Persist ALL contracts; no top-N. Snapshot every 5s starting 09:15:00 IST.
  - Tests: `test_option_movers_uses_full_packet_close_field`, `test_option_movers_persists_all_contracts`, `test_option_movers_starts_at_0915_sharp`
  - Schema: `option_movers (ts, security_id, segment, underlying, expiry, strike, option_type, ltp, prev_close, change_pct, oi, volume) DEDUP UPSERT KEYS(ts, security_id, segment)`.

- [ ] **Item 4: previous_close TABLE population (segment-aware routing per Ticket #5525125)**
  - Files: NEW `crates/storage/src/previous_close_persistence.rs`, `crates/core/src/pipeline/tick_processor.rs`
  - Routing matrix:
    | Segment | Source | Byte offset | Trigger |
    |---------|--------|-------------|---------|
    | IDX_I | PrevClose packet (code 6) | bytes 8-11 (f32 LE) | On packet receipt |
    | NSE_EQ | Quote packet (code 4) `close` field | bytes 38-41 (f32 LE) | First Quote per security_id per day |
    | NSE_FNO | Full packet (code 8) `close` field | bytes 50-53 (f32 LE) | First Full per security_id per day |
  - Schema: `previous_close (trading_date, security_id, segment, prev_close, source) DEDUP UPSERT KEYS(trading_date, security_id, segment)`.
  - Tests: `test_prev_close_routing_idx_i_from_code6`, `test_prev_close_routing_nse_eq_from_quote_close_field`, `test_prev_close_routing_nse_fno_from_full_close_field`, `test_prev_close_dedup_includes_segment`.

### P1 — RESILIENCE (root cause of overnight outage)

- [ ] **Item 5: Main-feed WebSocket NEVER GIVES UP**
  - Files: `crates/core/src/websocket/connection.rs:1256-1293`, `crates/core/src/websocket/connection_pool.rs`
  - Change: Replace post-close `return false` (line 1285) with `sleep_until_next_market_open_ist()` then continue retry loop forever. Remove the 3-attempt cap entirely outside market hours. Add supervisor `respawn_dead_connections_loop` in pool that re-spawns any task that exits with `ReconnectionExhausted`.
  - Tests: `test_main_feed_post_close_sleeps_until_next_open`, `test_pool_supervisor_respawns_dead_connection`, `test_no_reconnect_exhaustion_path_remains` (source scan)

- [ ] **Item 6: Depth-20, Depth-200, Order-Update WS NEVER GIVE UP**
  - Files: `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/websocket/order_update.rs`
  - Change: Same supervisor pattern. Depth's 60-attempt cap (`websocket-enforcement.md` rule 14) becomes "60 in-market attempts then sleep until 09:00". Order-update activity watchdog stays at 14400s (`depth-subscription.md` 2026-04-24 §7).
  - Tests: 3 × `test_<feed>_post_close_sleeps_until_next_open`, 3 × `test_<feed>_supervisor_respawns_dead_task`

- [ ] **Item 7: FAST BOOT race fix (QuestDB Broken pipe)**
  - Files: `crates/app/src/main.rs` (boot sequence), `crates/storage/src/tick_persistence.rs`
  - Change: Tick processor await on `questdb_health.wait_tcp_ready(deadline=10s)` before consuming SPSC. If timeout, ticks rescue to ring buffer silently for first 30s window; afterward escalate to ERROR.
  - Tests: `test_tick_processor_waits_for_questdb_ready`, `test_rescue_ring_silent_during_boot_window`

### P2 — VISIBILITY (operator confidence)

- [ ] **Item 8: Pre-open movers (09:00..=09:12 IST) for stocks + indices**
  - Files: `crates/core/src/pipeline/preopen_movers.rs` (NEW), `crates/app/src/main.rs`
  - Change: From 09:00 to 09:12, compute % change for every stock + index using pre-open buffer ticks vs `previous_close`. Persist to `stock_movers` (with `phase = 'PREOPEN'` column). Snapshot every 30s.
  - Tests: `test_preopen_movers_uses_buffer_ticks`, `test_preopen_movers_window_is_0900_to_0912_inclusive`

- [ ] **Item 9: Streaming heartbeat (09:15:30) + depth anchor (09:13:00) verification**
  - Files: `crates/app/src/main.rs`
  - Change: Confirm `MarketOpenStreamingConfirmation` and `MarketOpenDepthAnchor` fire today. Add `test_market_open_heartbeat_fires_once_per_trading_day` ratchet that scans the dispatcher for the call site (Rule 13: defined-but-not-called = bug).

- [ ] **Item 10: Tick-gap RCA for 406 stock F&O contracts**
  - Files: `crates/core/src/websocket/connection.rs`, `crates/core/src/instrument/subscription_planner.rs`
  - Change: Investigate yesterday's 09:18:45 log "tick feed gaps (30s summary): 406 instruments had gaps". Hypotheses: (a) Dhan-side per-instrument silent unsubscription, (b) `SubscribeRxGuard` regression, (c) batch-101 rule violation. Add per-connection `last_tick_seen_per_security_id` papaya map; emit `TickGapDetected` event after 60s silence per instrument.
  - Tests: `test_tick_gap_detected_after_60s_silence`, `test_subscribe_rx_guard_survives_overnight` (16h synthetic)

## 9-Box Observability Checklist (every item × 9 boxes = 90 cells)

| Item | Typed Event | ErrorCode | tracing+code | Prom counter | Grafana panel | Alert rule | Call site | Triage YAML | Ratchet test |
|------|-------------|-----------|--------------|--------------|---------------|------------|-----------|-------------|--------------|
| 1 Phase2 notify | ✓ exists | new `PHASE2-01` | ✓ | `tv_phase2_outcome_total{result}` | + panel | + rule | new | new | new |
| 2 Stock movers | new `MoversComputed` | `MOVERS-01` | ✓ | `tv_movers_persisted_total{kind}` | + panel | + rule | NEW (today missing!) | new | new |
| 3 Option movers | new `OptionMoversComputed` | `MOVERS-02` | ✓ | `tv_option_movers_persisted_total` | + panel | + rule | NEW | new | new |
| 4 Prev close | new `PrevCloseLoaded` | `PREVCLOSE-01` | ✓ | `tv_prev_close_persisted_total{source}` | + panel | + rule | NEW | new | new |
| 5 Main-feed resilience | reuse `WebSocketReconnected` | `WS-GAP-04` | ✓ | `tv_ws_post_close_sleep_total{feed}` | + panel | + rule | upgrade | new | new |
| 6 Depth/OU resilience | same | same | ✓ | same | + panel | + rule | upgrade | new | new |
| 7 FAST BOOT | new `BootRaceAvoided` | `BOOT-01` | ✓ | `tv_boot_race_rescue_total` | + panel | + rule | NEW | new | new |
| 8 Pre-open movers | new `PreopenMoversComputed` | `MOVERS-03` | ✓ | `tv_preopen_movers_total` | + panel | + rule | NEW | new | new |
| 9 Heartbeat verify | already exists | already | already | already | + ratchet | + alert if missing | verify | new | new |
| 10 Tick gap RCA | new `TickGapDetected` | `WS-GAP-05` | ✓ | `tv_tick_gap_total{security_id}` | + panel | + rule | NEW | new | new |

## Files Touched (estimate)

| Layer | Files | LoC delta |
|-------|-------|-----------|
| `crates/core/` parser + pipeline | 5 (option_movers NEW, top_movers, tick_processor, preopen_movers NEW, websocket/connection) | +800 |
| `crates/storage/` | 3 (movers_persistence, previous_close_persistence NEW, tick_persistence) | +400 |
| `crates/app/main.rs` boot wiring | 1 | +300 |
| `crates/common/` types + ErrorCode | 2 | +150 |
| `crates/core/src/notification/events.rs` | 1 | +120 |
| Tests | ~25 new test files / additions | +1500 |
| Grafana dashboards | 4 new panels in `operator-health.json` + new `movers.json` | +200 lines JSON |
| Alert rules | `deploy/docker/prometheus/alerts.yml` | +60 |
| Triage YAML | `.claude/triage/error-rules.yaml` | +40 |
| Runbooks | 6 new in `docs/runbooks/` | +600 |

## Order of Execution (proposed)

| Wave | Items | Why first |
|------|-------|-----------|
| **Wave 1 (P0 data-integrity)** | 4 (prev_close), 2 (stock movers persist), 3 (option movers), 1 (Phase2 notify) | Operator can't see anything today; must populate tables and emit events |
| **Wave 2 (P1 resilience)** | 5 (main-feed never give up), 6 (depth/OU never give up), 7 (FAST BOOT race) | Prevents the next overnight outage |
| **Wave 3 (P2 visibility)** | 8 (preopen movers), 9 (heartbeat verify), 10 (tick-gap RCA) | Confidence + future incident prevention |

## Verification Gates

| Gate | Command | Blocks merge? |
|------|---------|---------------|
| Plan items all checked | `bash .claude/hooks/plan-verify.sh` | yes |
| Scoped tests pass | `make scoped-check` | yes |
| Hot-path zero-alloc | `cargo test --test dhat_*` | yes |
| Banned-pattern scan | `bash .claude/hooks/banned-pattern-scanner.sh` | yes |
| Workspace test (since `common/` touched) | `cargo test --workspace` | yes |
| Real-time proof | `make doctor` shows all 4 WS connected + movers tables non-empty + previous_close populated | manual |

## Scenarios Covered

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App starts Sunday 22:00 IST, runs through to Monday 09:00 | All 4 WS connected automatically at 09:00 (no manual restart) |
| 2 | Network blip 02:00 IST (off-market) | Connections sleep until 09:00, then reconnect — NO Telegram spam |
| 3 | Phase 2 plan empty (e.g. all stocks missing pre-open price) | `Phase2Failed{...}` Telegram with diagnostic, NOT silent |
| 4 | 09:15:00 sharp | Option movers task starts; first snapshot persisted by 09:15:05 |
| 5 | NSE_EQ `RELIANCE` ticks Quote packet | `previous_close` row written for (RELIANCE, NSE_EQ) from byte 38-41 |
| 6 | NSE_FNO `NIFTY-Jun26-25000-CE` ticks Full packet | `previous_close` row written from byte 50-53 |
| 7 | IDX_I `NIFTY=13` ticks PrevClose packet (code 6) | `previous_close` row written from byte 8-11 |
| 8 | QuestDB Docker not yet up at boot | Tick processor sleeps 10s waiting; rescue ring silent during boot window |
| 9 | Connection 0 task crashes mid-day | Pool supervisor respawns it within 5s; SubscribeRxGuard restores subscriptions |
| 10 | Operator queries `select count(*) from stock_movers where ts > now()-1h` | Returns ~239 × 720 = 172,000 rows (every tracked stock × 5s snapshots) |
