# Zero-Undetected-Loss Program

**Status:** APPROVED (user: Parthiban, 2026-04-16 — "yes go ahead with everything bro fix everything dude")
**Date:** 2026-04-16
**Scope:** Ship the P0/P1/P2 items from the session-end zero-loss plan. Goal is **zero undetected loss** + **bounded recovery time** + **provable audit trail**, not the physically-impossible "100% guarantee".

## Guiding principle

**"Any single failure is detected within 50s, recovered automatically, logged to the audit trail, and reconciled against Dhan's REST API at end-of-day. Any unrecovered loss is paged to Telegram within 60s. Any silent corruption is caught by the morning reconciliation before the next trading session."**

## Plan Items (ranked by ROI)

### P0 — highest impact, ship first

- [x] **ZL-P0-1**: Tick heartbeat gauge (`tv_ws_last_frame_epoch_secs{ws_type,connection_id}`) — done
  - Files: `crates/core/src/websocket/activity_watchdog.rs` (helper), `connection.rs`, `depth_connection.rs` (x2 sites — depth-20 + depth-200), `order_update_connection.rs`
  - Tests: `test_zl_p0_1_heartbeat_metric_name_stable`, `test_zl_p0_1_heartbeat_gauge_builds_for_all_ws_types`, `test_zl_p0_1_heartbeat_gauge_accepts_extreme_values`
  - Impact: catches frozen event loop the watchdog cannot see.

- [x] **ZL-P0-2**: Per-underlying canary gauge for NIFTY/BANKNIFTY/SENSEX spots — done
  - Files: `crates/core/src/pipeline/tick_processor.rs`
  - Tests: `test_zl_p0_2_canary_underlyings_table_is_bounded`, `test_zl_p0_2_canary_underlyings_contains_nifty_banknifty_sensex`, `test_zl_p0_2_canary_underlyings_have_non_empty_labels`, `test_zl_p0_2_canary_metric_name_stable`, `test_zl_p0_2_canary_metric_builds_without_recorder`
  - Impact: end-to-end "alive but not flowing" detection.

- [ ] **ZL-P0-3**: End-of-day reconciliation — compare QuestDB tick counts vs Dhan historical REST candles
  - Files: `crates/core/src/reconciliation/end_of_day.rs` (new module), `crates/storage/src/reconciliation_persistence.rs` (new), boot wiring in `crates/app/src/main.rs`
  - Tests: `test_zl_p0_3_reconciliation_computes_mismatch_correctly`, `test_zl_p0_3_reconciliation_zero_ticks_fires_critical`, `test_zl_p0_3_reconciliation_writes_to_questdb`
  - Impact: the ONLY way to go from "likely zero loss" to "provably zero loss".

- [ ] **ZL-P0-4**: Dual live-feed redundancy — subscribe same instruments on 2 WS connections, merge + dedup by `(security_id, exchange_timestamp)` with 200ms window
  - Files: `crates/core/src/websocket/dedup_merger.rs` (new), `crates/core/src/websocket/connection_pool.rs`, `crates/app/src/trading_pipeline.rs`
  - Tests: `test_zl_p0_4_dedup_drops_duplicate_within_window`, `test_zl_p0_4_dedup_forwards_distinct_ticks`, `test_zl_p0_4_dedup_forwards_late_duplicate_after_window`, `test_zl_p0_4_bounded_dedup_map_eviction`
  - Impact: single-connection failure is no longer a data loss event.

### P1 — high value, ship after P0

- [x] **ZL-P1-1**: Audit `tick_persistence` WAL wiring — **PASS, no fix needed**
  - Evidence: WsFrameSpill created at boot (main.rs:334), attached to all connections via `new_with_optional_wal` (connection_pool.rs:176-178), WAL append happens BEFORE try_send (connection.rs:756-771), boot replay re-injects frames (main.rs:649-691), 100K burst + SIGKILL + corrupted-tail chaos tests all pass. Full audit in session notes.
  - Impact: ZL-P1-2 (extend WAL to depth + order) is ALSO already done — depth_connection and order_update_connection both receive `wal_spill: Option<Arc<WsFrameSpill>>` (main.rs:2483, 2638, 2872)

- [x] **ZL-P1-2**: WAL spill coverage for depth + order update — **ALREADY DONE**
  - Evidence: depth_connection receives `wal_spill` at main.rs:2483 (depth-20) and main.rs:2638 (depth-200). order_update_connection receives at main.rs:2872. All use same `spill.append(WsType::*, frame)` pattern as live feed. Chaos tests cover all 4 WS types.

### P2 — hardening, ship after P1

- [ ] **ZL-P2-1**: Chaos test harness (kill QuestDB, kill app, fill disk, cut network)
  - Files: `crates/core/tests/chaos_*.rs`, scripts
  - Tests: integration-style, run under `--test-threads=1`

- [ ] **ZL-P2-2**: QuestDB hot-standby replica
  - Files: `deploy/docker/docker-compose.yml`, config
  - Tests: deployment validation

- [ ] **ZL-P2-3**: Mutation testing ratchet on tick + oms paths
  - Files: CI config, `.claude/hooks/`
  - Tests: `cargo mutants` baseline

## Scenarios Covered

| # | Scenario | Detection | Recovery |
|---|----------|-----------|----------|
| 1 | Dhan WS disconnects | watchdog (50s) + disconnect code | reconnect (exponential backoff) |
| 2 | Dhan WS silent-alive (no data) | heartbeat gauge + canary (5s) | forced reconnect on Grafana alert |
| 3 | Read loop frozen | heartbeat gauge (5s) | process restart via health check |
| 4 | Watchdog panics | WS-1 panic supervisor | notify fires, read loop reconnects |
| 5 | QuestDB down short (<40min) | indicator_snapshot+movers rescue ring | auto-drain on reconnect |
| 6 | QuestDB down longer | rescue ring overflow metric → Telegram | operator intervention |
| 7 | App `kill -9` mid-tick | deep_depth fsync'd WAL | replay on restart |
| 8 | Silent tick gap not caught live | end-of-day reconciliation | morning backfill from Dhan REST |
| 9 | One WS connection hangs | dual-feed redundancy | second connection keeps flowing |
| 10 | Dedup map grows unbounded | bounded map eviction (test) | bounded memory |

## Out of scope (explicit)

- **"100% guarantee"** against network partition, region outage, hardware failure, cosmic ray bit-flips. These violate physics.
- **Sub-millisecond recovery time** — recovery is bounded at ~50s (watchdog threshold) + ~5s (reconnect) ≈ 1 minute worst case.
- **Formal verification** of concurrent code — we use loom for data-race checks, not TLA+ for full verification.
