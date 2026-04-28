# Wave 4 Worst-Case Catalog — Categories F + G + H + I + J

> **Authority:** companion to `.claude/plans/active-plan-wave-4.md`
> **Scope:** Wave-4-E3 sub-PR enumerates every scenario below into a
> chaos test + ErrorCode + ratchet.

## Category F — Time / calendar surprises (7 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| F1 | Trade attempted on declared NSE holiday | OMS pre-trade check vs `TradingCalendar` | Reject order, Telegram CRITICAL | TIME-01 | `chaos_trade_on_holiday.rs` |
| F2 | Holiday added intraday (NSE rare announcement) | Calendar refresh on next boot | If app running through the announcement, no auto-refresh — operator must restart | (operator runbook) | manual |
| F3 | Daylight-saving boundary (IST has none, but server clock might) | BOOT-03 clock-skew probe | HALT if skew > 2s | (existing BOOT-03) | covered |
| F4 | Year boundary (rollover 31-Dec → 01-Jan) | Calendar-driven trading-date computation | Pure function — handled by `chrono`; ratchet test pins behavior | (none new) | `test_calendar_year_boundary` |
| F5 | Stock F&O expiry rolls during market hours (rare — last Thursday issue) | Phase 2 already dispatched today's expiry; rollover threshold flips at midnight | Wave-4-A constant flip; rollover happens BEFORE 09:13 dispatch | (none new) | covered by Wave-4-A tests |
| F6 | Friday-after-holiday (4-day gap) | `secs_until_next_market_open` returns ~96h | WS-GAP-04 sleep-until-open accommodates arbitrary gap | (existing WS-GAP-04) | covered by Scenario 15 |
| F7 | Operator's wall clock manually rolled back | Boot-time IST sanity check (now > previous boot's wake_ts) | Detect; refuse to start; Telegram CRITICAL | TIME-02 (reserved, add to error_code.rs Wave-4-E3) | `chaos_clock_rollback.rs` |

## Category G — Resource exhaustion (5 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| G1 | File-descriptor count > 80% of ulimit | `tv_fd_count` gauge alert | Telegram WARN; if > 95%, HALT | RESOURCE-01 | `chaos_fd_exhaustion.rs` |
| G2 | Resident memory > 80% of cgroup limit | `tv_resident_memory_bytes` alert | Telegram WARN; if > 95%, HALT pre-emptively (better than OOM-kill) | RESOURCE-02 | `chaos_memory_pressure.rs` |
| G3 | Spill file > 50% of free disk | `tv_spill_file_size_bytes` alert | Telegram WARN; trigger spill-flush-to-questdb retry | RESOURCE-03 | `chaos_spill_growth.rs` (overlaps C5) |
| G4 | tokio worker thread starvation (long-running blocking call) | `tokio-console` task latency > 100ms | Convert to `spawn_blocking`; pre-existing CI bench-gate guards hot path | (none new — pre-existing) | bench-gate enforces |
| G5 | Allocator fragmentation (RSS bloat without leak) | RSS / heap-used ratio > 2x | Telegram WARN; periodic mem-trim trigger | (none new) | observed in QuestDB tuning |

## Category H — Operator error (6 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| H1 | Dhan client-id changed in config (typo or wrong account) | Boot-time SSM canonical comparison | Refuse to start; Telegram CRITICAL | OPER-01 | `chaos_client_id_drift.rs` |
| H2 | Wrong env config selected (prod toml on dev host) | Boot-time hostname vs config `expected_hostname` | Refuse to start | OPER-02 (reserved) | `chaos_wrong_env_config.rs` |
| H3 | Manual `cargo update` ran (banned per CLAUDE.md) | CI pre-merge `Cargo.lock` git diff vs main | Block merge | (existing CI) | covered |
| H4 | `.env` file committed accidentally | Pre-commit secret-scan hook | Block commit | (existing hook) | covered |
| H5 | Manual order placed via Dhan web app concurrent with our OMS | Live Order Update WS `Source: "N"` (Normal/web) | Acknowledge, do NOT treat as our order, log | (covered by `Source` filter) | `chaos_concurrent_web_order.rs` |
| H6 | `--no-verify` git push attempted | `pre-push` hook unconditionally enforced | Hook can be bypassed by `git push --no-verify`; ratchet via CI re-run on push event | (existing CI fallback) | none — CI catches |

## Category I — Resilience cascade (4 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| I1 | Dual-instance running same client-id | `live_instance_lock` UPSERT on boot | Second instance HALTs cleanly | RESILIENCE-01 | `chaos_dual_instance.rs` |
| I2 | App crashes mid-`Swap20` rebalance | `subscription_audit` row sequence incomplete on next boot | Replay-on-boot re-issues the unfinished swap | RESILIENCE-02 | `chaos_mid_rebalance_crash.rs` |
| I3 | Triple failure (WS drop + QuestDB pause + token expiry within 60s) | All 3 alerts fire within window | Each recovery primitive runs independently — no special path | CASCADE-01 (synthetic test) | `chaos_triple_failure.rs` |
| I4 | Recovery storm (after long outage, every subsystem retries simultaneously) | Boot-time staggered start: WS first, then depth, then historical | Built-in via boot sequence ordering | (none new) | `chaos_recovery_storm.rs` |

## Category J — Idempotency breaches (5 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| J1 | Same UUID v4 idempotency key returns 2 distinct orderIds | OMS reconciliation diff | Halt OMS; Telegram CRITICAL; manual reconciliation required | IDEMP-01 | `chaos_order_idempotency_breach.rs` |
| J2 | Same `(security_id, segment, expiry)` written with different lot_size | DEDUP UPSERT collision detected | Last-writer-wins per QuestDB DEDUP; Telegram WARN | IDEMP-02 | `chaos_instrument_master_drift.rs` |
| J3 | Phase 2 fires twice on same `trading_date_ist` | `Phase2EmitGuard` (Wave-1) catches duplicate | First fire's outcome wins; second drops with Telegram | IDEMP-03 | covered by Wave-1 ratchet |
| J4 | Same `(underlying, levels, ts)` swap-event published twice | `depth_rebalance_audit` DEDUP UPSERT | Last-writer-wins; counter increment | IDEMP-04 | `chaos_depth_rebalance_double_fire.rs` |
| J5 | `historical_fetch_state.last_success_date` regresses (moved backwards) | Idempotency guard at Wave-4-B commit 4 write-site | Refuse the regression write; Telegram WARN | IDEMP-05 | `chaos_historical_state_regression.rs` |

## Trigger

This file activates when editing:
- `crates/trading/src/oms/idempotency.rs` (J1)
- `crates/storage/src/historical_fetch_state_persistence.rs` (J5)
- `crates/core/src/instrument/phase2_emit_guard.rs` (J3)
- Any chaos test matching F1–F7 / G1–G5 / H1–H6 / I1–I4 / J1–J5
