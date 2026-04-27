# Gap Closure (G1–G21)

> Identified during plan review 2026-04-27. **G1, G14, G15 verified by 3 background agents 2026-04-27.** Each gap maps to a specific item and a specific ratchet.

| # | Gap | Verification status | Plan-item home | New ratchet test |
|---|-----|---------------------|----------------|------------------|
| G1 | Token-refresh ↔ WS-sleep coordination — 65.5h sleep on Fri→Mon exceeds 24h JWT, no force-renewal exists | **VERIFIED** by agent 3 — `TokenManager::force_renewal()` and `next_renewal_at()` ABSENT; `wait_for_valid_token` polls 60s then reconnects with stale token anyway | Item 5 | `test_token_force_renewal_on_wake`, `test_70h_sleep_then_connect_succeeds` |
| G2 | First-Quote/Full-per-day detection needs IST-midnight reset — long-running process never re-populates prev_close after holiday | UNVERIFIED — no such reset exists today (agent 1 confirms first-seen mechanism is absent entirely) | Item 4 | `test_first_seen_set_resets_at_ist_midnight` |
| G3 | `trading_date` timezone in DEDUP key for `previous_close` — UTC vs IST silently differ at IST 00:00–05:30 | LOGICAL gap — must be IST (Asia/Kolkata) | Item 4 | `test_trading_date_ist_not_utc` |
| G4 | `TickGapDetected` flooding — 406 instruments × 1 Telegram each | LOGICAL gap | Item 8 + Item 11 (coalescer) | `test_tick_gap_telegram_coalesced_per_60s` |
| G5 | NSE_FNO close-field availability depends on subscription MODE — Quote vs Full | LOGICAL gap; current plan assumes all FNO are Full | Item 4 (routing matrix 4th row) | `test_prev_close_routes_via_quote_close_when_fno_subscribed_quote_mode` |
| G6 | Rollback-flag tests for C9 — feature flags without flip-to-off test are hopes | LOGICAL gap | All 14 items | `feature_flag_rollback_guard.rs` (one entry per flag) |
| G7 | Phase2 "exactly ONE event" — no enforcement | LOGICAL gap | Item 1 (`Phase2EmitGuard` Drop type) | `test_phase2_emit_guard_panics_in_debug_when_dropped` |
| G8 | Clock-skew tolerance for 09:13/09:15:30/09:16 triggers — no NTP check | LOGICAL gap | Item 7 + boot probe | `test_boot_probe_emits_critical_on_clock_skew_gt_2s` |
| G9 | Item 3 ILP-flush cadence vs 5s snapshot — could lag 6× | LOGICAL gap | Item 3 (`config.questdb.ilp.flush_interval_ms ≤ 1000`) | `test_questdb_writeback_lag_p99_le_2s` |
| G10 | `MarketOpenSelfTestFailed` halt policy — no kill-switch wiring spec | LOGICAL gap | Item 12 | `test_self_test_failed_activates_kill_switch` |
| G11 | Disaster-recovery doc stale after sleep-until-open ships — scenarios 5/6/7/8 | LOGICAL gap | C11 cross-cutting | rewrite scenarios + add 12/13 |
| G12 | Banned-pattern hook self-test for new patterns — hook silently broken | LOGICAL gap | C10 cross-cutting | `.claude/hooks/test-fixtures/` + CI step |
| G13 | Greeks pipeline ↔ option_movers ordering — shared broadcast subscriber? | LOGICAL gap | Item 3 | `bench_greeks_lag_with_movers_running_le_50ms_p99` |
| G14 | Auto-up bootstrap ↔ FAST BOOT race — fixed 10s deadline insufficient for cold Docker pull | **VERIFIED** by agent 2 (bonus finding — `std::fs::create_dir_all` per packet); plus G14 logical analysis from session-start hook (PR #384) | Item 7 (60s deadline + escalating logs) | `test_wait_for_questdb_ready_60s_with_escalation` |
| G15 | Telegram rate-limit + coalescing accounting | **VERIFIED** by agent 3 — bucket ABSENT, coalescer ABSENT, drop counter ABSENT, unbounded `tokio::spawn` per event | Item 11 (NEW) | 5 tests in Item 11 |
| G16 | Movers `compute_snapshot` complexity bound — 22K options × sort = O(N log N) at 5s cadence | LOGICAL gap | Item 3 (`bench_compute_snapshot_22k_options_le_5ms`) | bench |
| G17 | `option_movers.expiry/strike` lookup must stay O(1) | LOGICAL gap | Item 3 (papaya `get_with_segment` ≤ 50ns) | `bench_option_mover_enrichment_lookup_le_50ns` |
| G18 | Glacier Deep Archive 90-day minimum vs partition churn | LOGICAL gap | C13 + Item 9 partition manager | `test_partition_manager_never_recreates_archived_partition` |
| G19 | `TickGapDetected` papaya map memory bound — entries never expire after market close | LOGICAL gap | Item 8 (15:35 IST reset) | `test_tick_gap_map_resets_at_1535_ist` |
| G20 | `make 100pct-audit` exit-code semantics — runtime vs CI | LOGICAL gap | Item 13 | `test_100pct_audit_exits_0_on_runtime_pass`, `test_100pct_audit_ci_reads_snapshot` |
| G21 | Pre-push gate scoping — 9,620 LoC touches `crates/common/` → forces FULL_QA | LOGICAL gap | Plan footer (Verification Gates) | `FULL_QA=1` mandate documented |

## G-level summary

- **Verified by agent runs:** G1, G14 (bonus), G15 — all 3 agents corroborated
- **Logical gaps identified by review:** 18 (G2-G13, G16-G21 minus G14, G15)
- **Total ratchet tests added by G-closures:** ~25 tests beyond what the original 10-item plan included
- **Total LoC added by G-closures:** ~1,200 (mostly tests + 200 LoC config + 300 LoC docs)

## Cross-reference: which file touches which gap

| File | Gaps closed |
|---|---|
| `crates/core/src/auth/token_manager.rs` | G1 |
| `crates/core/src/pipeline/first_seen_set.rs` (NEW) | G2 |
| `crates/storage/src/previous_close_persistence.rs` (NEW) | G3, G5 |
| `crates/core/src/notification/telegram.rs` | G4, G15 |
| `crates/app/tests/feature_flag_rollback_guard.rs` (NEW) | G6 |
| `crates/app/src/main.rs` Phase2 dispatcher | G7 |
| `crates/app/src/infra.rs` boot probe | G8 |
| `config/base.toml` ILP flush + features | G9, C9 |
| `crates/trading/src/risk/kill_switch.rs` | G10 |
| `.claude/rules/project/disaster-recovery.md` | G11 |
| `.claude/hooks/test-fixtures/` (NEW) | G12 |
| `crates/trading/benches/greeks_lag.rs` | G13 |
| `crates/storage/src/tick_persistence.rs` boot wait | G14 |
| `crates/core/src/pipeline/option_movers.rs` (NEW) | G16, G17 |
| `crates/storage/src/partition_manager.rs` | G18 |
| `crates/core/src/pipeline/tick_gap_detector.rs` (NEW) | G19 |
| `scripts/100pct-audit.sh` (NEW) | G20 |
| `active-plan.md` Verification Gates | G21 |
