# Active Plan — Wave 4 (final hardening, post-Wave-3-D)

**Status:** DRAFT
**Date:** 2026-04-28
**Approved by:** pending
**Branch:** `claude/review-project-rules-EpTDM` (plan only) → sub-PRs branch from main
**Charter:** `.claude/rules/project/wave-4-shared-preamble.md` (auto-loaded)
**Precondition:** Wave-3-D (PR #405) merged to main — VERIFIED 2026-04-28.

This file is the canonical Wave 4 plan. Every Wave 4 sub-PR session
(Wave-4-A through Wave-4-E3) reads the shared preamble + this file +
the matching SCOPE block below. SCOPE blocks expand into a per-sub-PR
`active-plan.md` (Status: DRAFT → APPROVED → VERIFIED) when each
session begins, per `.claude/rules/project/plan-enforcement.md`.

## Plan Items (14, FROZEN 2026-04-28)

- [ ] **1. Stock F&O expiry rollover threshold `<= 1` → `== 0`** (Wave-4-A)
  - Files: `crates/core/src/instrument/subscription_planner.rs`, `crates/common/src/constants.rs`
  - Tests: `test_stock_expiry_keeps_nearest_on_t_minus_1`, `test_stock_expiry_rolls_only_on_t`, `test_index_expiry_never_rolls_via_planner`, `test_stock_expiry_rollover_threshold_constant_is_zero`, `test_count_trading_days_expiry_day_from_t_minus_1_returns_one`

- [ ] **2. DH-904 retry capped at 10 attempts per (timeframe × instrument × day)** (Wave-4-B commit 1)
  - Files: `crates/core/src/historical/candle_fetcher.rs`, `crates/common/src/error_code.rs`
  - Tests: `test_dh904_retry_caps_at_10_attempts`, `test_dh904_eleventh_attempt_emits_hist04`, `test_hist04_severity_is_high`

- [ ] **3. Historical fetch runs ONLY post-market (15:30+ IST) on trading days** (Wave-4-B commit 2)
  - Files: `crates/app/src/main.rs`, `crates/core/src/historical/scheduler.rs` (NEW)
  - Tests: `test_no_boot_fetch_on_trading_day`, `test_post_market_scheduler_fires_at_1530_ist`, `test_scheduler_idempotent_within_same_day`

- [ ] **4. Holiday/weekend boot-time fetch + 09:00 IST fire if app survives the holiday** (Wave-4-B commit 3)
  - Files: `crates/core/src/historical/scheduler.rs`, `crates/common/src/trading_calendar.rs`
  - Tests: `test_boot_fetch_on_holiday`, `test_boot_fetch_on_weekend`, `test_holiday_scheduled_fire_at_0900_ist`

- [ ] **5. Incremental "today only" + multi-day gap re-fetch** (Wave-4-B commit 4)
  - Files: `crates/storage/src/historical_fetch_state_persistence.rs` (NEW), `crates/core/src/historical/candle_fetcher.rs`, `crates/storage/src/instrument_persistence.rs` (DDL)
  - Tests: `test_first_fetch_lands_90_days`, `test_subsequent_fetch_today_only`, `test_multi_day_outage_refetches_gap`, `test_historical_fetch_state_dedup_key_includes_segment`

- [ ] **6. Cross-verify 100% match + skip-on-fresh + once-per-day post-market** (Wave-4-B commit 5)
  - Files: `crates/core/src/historical/cross_verify.rs`, `crates/common/src/constants.rs`
  - Tests: `test_cross_match_threshold_is_100pct_exact`, `test_cross_match_skipped_when_live_ticks_zero`, `test_cross_match_runs_once_per_trading_day`

- [ ] **7. Boot-state matrix `boot_decision()` pure-function module** (Wave-4-B commit 6)
  - Files: `crates/core/src/instrument/boot_decision.rs` (NEW)
  - Tests: `test_boot_decision_cold_premarket_trading_day`, `test_boot_decision_cold_midmarket_trading_day`, `test_boot_decision_cold_postmarket_trading_day`, `test_boot_decision_weekend`, `test_boot_decision_holiday`, `test_boot_decision_slow_boot_questdb_lag`, `test_boot_decision_hot_restart`, `test_boot_decision_app_survived_holiday`

- [ ] **8. Pre-open heartbeat at 09:00:30 IST + reaffirm 09:15:30 F&O heartbeat** (Wave-4-C commit 1)
  - Files: `crates/core/src/instrument/preopen_self_test.rs` (NEW), `crates/core/src/notification/events.rs`, `crates/common/src/error_code.rs`
  - Tests: `test_preopen_heartbeat_fires_at_0900_30_ist`, `test_preopen_self_test_validates_idx_i_and_nse_eq`, `test_market_open_streaming_heartbeat_unchanged`, `test_selftest_03_severity_routing`

- [ ] **9. Depth-20 dynamic top-150 OPTION CONTRACTS, 5-min edge-triggered swap** (Wave-4-D)
  - Files: `crates/core/src/instrument/depth_20_dynamic_subscriber.rs` (NEW), `crates/core/src/websocket/depth_connection.rs`, `crates/app/src/main.rs`
  - Tests: `test_dynamic_top_150_filters_to_options_only`, `test_dynamic_top_150_excludes_futidx_futstk_equity_index`, `test_dynamic_sort_top_volume_then_gainer_pct_desc`, `test_dynamic_distribution_3_conns_50_each`, `test_dynamic_resubscribe_every_5_min`, `test_dynamic_swap_edge_triggered_only_on_set_change`, `test_sensex_explicitly_skipped_bse_fno_unsupported`, `test_dynamic_swap_uses_depthcommand_swap20_zero_disconnect`, `test_dhat_zero_alloc_on_dynamic_select`

- [ ] **10. AWS schedule 08:00–17:00 IST weekday + 08:00–13:00 IST weekend (no 06:00 wake)** (Wave-4-C commit 2)
  - Files: `deploy/aws/eventbridge-schedule.yml`, `.claude/rules/project/aws-budget.md`
  - Tests: `test_eventbridge_cron_weekday_start_is_0800_ist`, `test_eventbridge_cron_weekday_stop_is_1700_ist`, `test_eventbridge_cron_weekend_window_or_skip`, `test_aws_budget_projection_under_5000_inr`

- [x] **11. Extreme Automation Charter** — DONE in `.claude/rules/project/wave-4-shared-preamble.md`

- [x] **12. Depth-200 Python SDK pin** — DONE in PR #340

- [x] **13. 15:35 IST tick-gap detector daily reset** — DONE in Wave-2-D

- [ ] **14. Worst-case scenario catalog (50+ scenarios, 10 categories A–J)** (Wave-4-E1/E2/E3)
  - Sub-items 14.1 (E1) — categories A+B; 14.2 (E2) — C+D+E; 14.3 (E3) — F+G+H+I+J. See SCOPE blocks below.

## Wave Sequencing

```
PHASE 0 (DONE)
  Wave-3-D merged to main (PR #405). Precondition satisfied.

PHASE 1 — 4 PARALLEL sub-PRs
  Spawn 4 fresh sessions concurrently:
    Session A → Wave-4-A (rollover)        ~30 LoC,  isolated
    Session B → Wave-4-B (historical)      ~1,200 LoC, isolated subdir
    Session C → Wave-4-C (heartbeats+AWS)  ~200 LoC
    Session D → Wave-4-D (depth-20 dyn)    ~600 LoC

  Conflict surface: crates/app/src/main.rs (boot wiring),
  crates/common/src/error_code.rs (each adds new variants),
  crates/core/src/notification/events.rs. 2nd/3rd/4th to merge
  rebase; ~5 min cost per PR.

PHASE 2 — 3 SEQUENTIAL sub-PRs (Wave-4-E1 → E2 → E3)
  All extend ErrorCode + audit_*_persistence + alerts.yml +
  dashboards. Parallel = guaranteed conflicts. Run sequentially.
```

## Wave-4-A SCOPE — Stock F&O expiry rollover (Item 1)

| Aspect | Value |
|---|---|
| Files | `crates/core/src/instrument/subscription_planner.rs`, `crates/common/src/constants.rs` |
| Constant | `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS = 0` (was `1`) |
| Decision table | T-2 keep nearest; T-1 keep nearest; T morning roll on or after market open. Indices unchanged (never roll). |
| Tests | 5 listed under Plan Item 1 above |
| Doc updates | `.claude/rules/project/depth-subscription.md` "Stock F&O Expiry Rollover" section, `docs/runbooks/expiry-day.md` |
| ErrorCode | none new |
| Prom / Grafana / alert | none new |
| 9-box | typed event N/A; runbook update; ratchet test count = 5 |
| Verification | `cargo test -p tickvault-core --lib instrument::subscription_planner`, then `FULL_QA=1 make scoped-check`, then 3 adversarial agents |

## Wave-4-B SCOPE — Historical fetch refactor (Items 2–7, 6 commits)

Commit 1 — DH-904 retry cap
| Aspect | Value |
|---|---|
| Files | `crates/core/src/historical/candle_fetcher.rs`, `crates/common/src/error_code.rs`, `.claude/rules/project/wave-4-error-codes.md` (NEW) |
| ErrorCode | `HIST-04 — DH-904 retry budget exhausted` (Severity::High, auto-triage NO) |
| Prom | `tv_historical_dh904_retries_total{tf,instrument}`, `tv_historical_hist04_total` |
| Telegram | `HistoricalFetchHist04 { tf, security_id, attempts }` |

Commit 2 — Post-market scheduler
| Aspect | Value |
|---|---|
| Files | NEW `crates/core/src/historical/scheduler.rs`, `crates/app/src/main.rs` |
| Change | Remove `run_historical_fetch_at_boot` on trading days. New scheduler fires at 15:30 IST. Idempotent per `(trading_date_ist)`. |
| Prom | `tv_historical_scheduler_fires_total`, `tv_historical_scheduler_skipped_idempotent_total` |

Commit 3 — Holiday/weekend handling
| Aspect | Value |
|---|---|
| Files | `crates/core/src/historical/scheduler.rs`, `crates/common/src/trading_calendar.rs` |
| Logic | If today is non-trading day → boot fetch; if app survives a non-trading day, schedule 09:00 IST fire next non-trading-day boot equivalent. |

Commit 4 — Incremental + gap re-fetch
| Aspect | Value |
|---|---|
| Files | NEW `crates/storage/src/historical_fetch_state_persistence.rs`, `crates/core/src/historical/candle_fetcher.rs` |
| Table | `historical_fetch_state(security_id, segment, timeframe, last_success_date_ist, ts) DEDUP UPSERT KEYS(security_id, segment, timeframe)` |
| Logic | First fetch = 90 days; subsequent fetches = `last_success_date_ist + 1 → today`. Multi-day gap → re-fetch the gap. |

Commit 5 — Cross-verify 100% + skip-on-fresh
| Aspect | Value |
|---|---|
| Files | `crates/core/src/historical/cross_verify.rs`, `crates/common/src/constants.rs` |
| Constants | already `CROSS_MATCH_PRICE_EPSILON = 0.0`. Reaffirm + add `CROSS_MATCH_SKIP_ON_ZERO_LIVE_TICKS = true` |
| Telegram | `CandleCrossMatchSkipped { reason: "live_ticks_zero" }` (new) |

Commit 6 — `boot_decision()` pure module
| Aspect | Value |
|---|---|
| Files | NEW `crates/core/src/instrument/boot_decision.rs` |
| Public API | `pub fn boot_decision(now_ist: NaiveDateTime, calendar: &TradingCalendar, last_run_state: &LastRunState) -> BootDecision` |
| Variants | `ColdPreMarket`, `ColdMidMarket`, `ColdPostMarket`, `Weekend`, `Holiday`, `SlowBoot`, `HotRestart`, `AppSurvivedHoliday` |
| Tests | 8 listed under Plan Item 7 |

## Wave-4-C SCOPE — Pre-open heartbeat + AWS schedule (Items 8, 10)

Commit 1 — Pre-open self-test
| Aspect | Value |
|---|---|
| Files | NEW `crates/core/src/instrument/preopen_self_test.rs`, `crates/core/src/notification/events.rs`, `crates/common/src/error_code.rs`, `.claude/rules/project/wave-4-error-codes.md` |
| Trigger | 09:00:30 IST once per trading day |
| Sub-checks | IDX_I feed has tick within 30s; NSE_EQ pre-open buffer has at least 1 entry per F&O stock; depth-20 connections in `Connected` state. |
| Telegram | `PreopenSelfTestPassed` (Info, edge-triggered recovery) / `PreopenSelfTestDegraded` (High) / `PreopenSelfTestCritical` (Critical) |
| ErrorCode | `SELFTEST-03 — pre-open self-test failed` |

Commit 2 — AWS schedule update
| Aspect | Value |
|---|---|
| Files | `deploy/aws/eventbridge-schedule.yml`, `.claude/rules/project/aws-budget.md` |
| Cron | weekday start `cron(30 2 ? * MON-FRI *)` (08:00 IST), stop `cron(30 11 ? * MON-FRI *)` (17:00 IST); weekend `cron(30 2 ? * SAT,SUN *)` start, `cron(30 7 ? * SAT,SUN *)` stop (or skip weekends per operator preference). |
| Cost | Recompute monthly bill; update `aws-budget.md` table (must remain ≤ ₹5,000/mo). |

## Wave-4-D SCOPE — Depth-20 dynamic top-150 option-only (Item 9)

| Aspect | Value |
|---|---|
| Files | NEW `crates/core/src/instrument/depth_20_dynamic_subscriber.rs`, `crates/core/src/websocket/depth_connection.rs`, `crates/app/src/main.rs` |
| Filter | `instrument_type IN (OPTIDX, OPTSTK) AND segment == NSE_FNO`. Excludes FUTIDX, FUTSTK, EQUITY, INDEX. |
| Sort | `(volume DESC, gainer_pct DESC)`, take top 150. |
| Distribution | 3 depth-20 connections × 50 = 150. Conn-1 + Conn-2 reserved for NIFTY + BANKNIFTY ATM±24 (existing). |
| SENSEX | EXPLICITLY SKIPPED — Dhan blocks BSE_FNO depth. SENSEX retains 5-level depth via main feed Full packet. Ratchet test pins this. |
| Refresh | Every 5 min during market hours (09:15–15:30 IST). Edge-triggered: only swap when set changes vs last cycle. |
| Mechanism | `DepthCommand::Swap20 { add: Vec<SecurityId>, remove: Vec<SecurityId> }` zero-disconnect. |
| ErrorCode | `DEPTH-DYN-01 — top-150 selector empty`, `DEPTH-DYN-02 — swap command channel broken` |
| Prom | `tv_depth_20_dynamic_swaps_total`, `tv_depth_20_dynamic_set_size`, `tv_depth_20_dynamic_skipped_no_change_total` |
| Grafana | NEW panel in `depth-flow.json` for set churn rate |
| DHAT / Criterion | zero-alloc on hot path; budget `select_top_150_le_50us` |
| Tests | 9 listed under Plan Item 9 |

## Wave-4-E1 SCOPE — Worst-case A + B (process/container/network)

| Aspect | Value |
|---|---|
| Categories | A. Process/container (7 scenarios) ; B. Network (4 scenarios) |
| New Prom counters | `tv_oom_kills_total` (cgroup memory.events scrape), `tv_ip_changed_total` (wired to `crates/core/src/network/ip_monitor.rs`), `tv_app_image_sha_changed_total` |
| New tables | `app_image_audit(boot_ts, image_sha, git_commit, version)` DEDUP UPSERT KEYS(boot_ts, image_sha) |
| New ErrorCodes | `PROC-01` OOM kill detected, `PROC-02` container restart loop, `NET-01` IP changed mid-session, `NET-02` DNS resolution failure cascade |
| Chaos tests | `chaos_oom_kill_recovery.rs`, `chaos_container_restart_loop.rs`, `chaos_ip_change_mid_session.rs`, `chaos_dns_resolution_storm.rs`, `chaos_app_image_drift.rs` |
| Doc | `.claude/rules/project/wave-4-error-codes.md` (NEW) — runbook for each |

## Wave-4-E2 SCOPE — Worst-case C + D + E (storage/auth/Dhan-API)

| Aspect | Value |
|---|---|
| Categories | C. Storage (6) ; D. Auth (4) ; E. Dhan-API (6) |
| New Prom counters | `tv_disk_full_total`, `tv_subscribed_no_data_total{security_id}` (gauge with TTL), `tv_token_external_rotation_detected_total` |
| New ErrorCodes | `STORAGE-GAP-05` disk-full pre-flight failed, `AUTH-GAP-04` TOTP secret rotated externally, `DH-911` Dhan API silent black-hole (subscribe accepted, no data) |
| Chaos tests | `chaos_disk_full_during_spill.rs`, `chaos_questdb_partition_corruption.rs`, `chaos_token_external_rotation.rs`, `chaos_dhan_subscribe_blackhole.rs`, `chaos_dhan_partial_response.rs` |

## Wave-4-E3 SCOPE — Worst-case F + G + H + I + J

| Aspect | Value |
|---|---|
| Categories | F. Time/calendar (7) ; G. Resource (5) ; H. Operator error (6) ; I. Resilience cascade (4) ; J. Idempotency (5) |
| New Prom counters | `tv_trades_on_declared_holiday_total`, `tv_fd_count` (gauge), `tv_resident_memory_bytes` (gauge), `tv_spill_file_size_bytes` (gauge), `tv_dhan_client_id_changed_total`, `tv_dual_instance_detected_total` |
| New ErrorCodes | `TIME-01` trade attempted on declared holiday, `RESOURCE-01..03` (FD / mem / spill thresholds), `OPER-01` client-ID changed in config, `RESILIENCE-01` dual-instance detected, `RESILIENCE-02` mid-rebalance crash recovered, `CASCADE-01` triple-failure recovery, `IDEMP-01..05` idempotency guard breaches |
| Chaos tests | 8 (one per cascade scenario) |
| Recovery primitive | `subscription_audit` replay-on-boot for mid-rebalance crash; `dual_instance_detector` via QuestDB UPSERT-on-host-id |

## How to deliver

For each Wave 4 sub-PR, paste ONE message into a fresh Claude Code session:

```
Implement Wave-4-X per .claude/plans/active-plan-wave-4.md
```

The shared preamble auto-loads at session start (it lives under
`.claude/rules/project/`). The plan auto-loads via the SCOPE
reference. Session writes its own per-sub-PR `active-plan.md` with
Status: DRAFT → APPROVED → VERIFIED before push, per
`plan-enforcement.md`.

## Final State Guarantees (after Wave 4 merges)

| Guarantee | Mechanism |
|---|---|
| Stock F&O rolls only on expiry day T (not T-1) | Wave-4-A constant flip + 5 ratchet tests |
| Historical fetch never on boot (trading day) | Wave-4-B post-market scheduler + boot-decision matrix |
| Cross-verify is bit-exact 100% match | Wave-4-B threshold + skip-on-fresh |
| AWS runs 08:00–17:00 IST only | Wave-4-C cron + budget projection |
| Depth-20 dynamic top-150 options refresh every 5 min | Wave-4-D edge-triggered resubscribe |
| 50+ worst-case scenarios mechanically guarded | Wave-4-E1/E2/E3 chaos tests + ratchets |
| Every Claude Code session inherits the charter | `wave-4-shared-preamble.md` auto-load |
| Honest 100% claim wording forced in every PR body | preamble Section 8 |

## Verification

```bash
bash .claude/hooks/plan-verify.sh        # this plan is reference-only; verify is per-sub-PR
ls .claude/rules/project/wave-4-shared-preamble.md   # must exist
ls .claude/plans/active-plan-wave-4.md               # must exist
```
