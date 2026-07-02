# Adversarial Permutation-Coverage Audit — 2026-07-01

## Honest envelope

This is a **code-and-design-level** adversarial permutation audit of the tickvault trading
system. Every permutation was checked against real source files, tests, ratchets, runbooks,
and Terraform in the repo, and every claimed gap was independently re-verified (refutation
pass) against the live tree before being counted. Findings therefore reflect what the
committed code and infrastructure-as-code actually prove — **not** live-box runtime
behaviour. No AWS read key / live-instance telemetry was available during this pass, so
runtime-only questions (does the box actually page at 09:25? does the EIP actually survive a
real stop→modify→start? does an OOM actually fire the systemd `failed` transition on the
running r8g.large?) remain **PENDING live verification**. Severities on the surviving gaps
use the refutation pass's `revised_severity` where a verdict lowered/raised the original
call. `dry_run = true` is in force (no live orders for ~3 months), which mitigates every
order-rejection-class gap to a pre-live-trading requirement rather than an active financial
risk.

## Summary by dimension

| Dimension | Permutations | Covered | Real gaps (survived) | Refuted (false gaps) |
|---|---:|---:|---:|---:|
| network-transport | 16 | 11 | 2 (NT-05*, NT-15) | 2 (NT-07, NT-16) — *NT-05 downgraded not refuted |
| auth-token | 16 | 8 | 4 (AUTH-P11, AUTH-P12, AUTH-P13, AUTH-P16*) | AUTH-P14, AUTH-P16* refuted (P16 rescoped low) |
| questdb-persist-wal | 17 | 12 | 1 (QPW-13*) | 4 (QPW-12, QPW-14, QPW-15, QPW-16) |
| feed-toggle-coldstart-races | 18 | 15 | 2 (FTC-14, FTC-15) | 0 |
| clock-calendar-longsleep | 17 | 11 | 3 (CCL-02, CCL-06, — plus verified list) | CCL-10, CCL-17 refuted |
| boot-process-oom-restartloop | 17 | 9 | 8 (BP-07, BP-08, BP-09, BP-13, BP-14, BP-17 + unverified BP-others) | BP-06 refuted |

\* NT-05: the "silent" black-hole premise was refuted (a purpose-built `no_tick_watchdog`
detects+pages), but a narrow residual survives (Dhan has no data-silence forced-reconnect,
unlike Groww FEED-STALL-01) → tracked at **low**.
\* AUTH-P16: refuted as a coverage gap (RESILIENCE-01 detect-and-halt covers it) → residual
atomicity hardening only, **low**.
\* QPW-13: the reject-gate artifact + `StorageGap05` ErrorCode are genuinely absent, but the
data-loss HARM is already covered by the write-failure→DLQ path → **low**.

> Note: the `boot-process-oom-restartloop` dimension's verified[] list only re-checked BP-06
> (refuted). BP-07/08/09/13/14/17 carry no refutation verdict and are counted as **real gaps
> as reported** (status=gap). They should be independently re-verified before closing.

---

## Covered matrix

| Dimension | Scenario | Class | Evidence (file :: test/anchor) |
|---|---|---|---|
| network-transport | NT-01 Bare TCP-RST storm reconnect-in-place | scenario | `crates/app/src/main.rs::is_bare_reset_class` / `test_is_bare_reset_class_all_benign_true` (main.rs:8992) |
| network-transport | NT-02 HTTP 429 cross-restart cooldown | scenario | `crates/core/src/websocket/rate_limit_cooldown.rs::test_remaining_cooldown_ms_active` (WS-GAP-08) |
| network-transport | NT-03 QuestDB probe damping vs 429 storm | bug | `crates/app/src/main.rs::damp_questdb_exit_signal` / `test_damp_questdb_exit_signal_threshold_is_two` (main.rs:9048) |
| network-transport | NT-04 15-min reconnect-in-place ceiling | condition | `crates/app/src/main.rs::reconnect_in_place_ceiling_exceeded` / `test_reconnect_in_place_ceiling_over_exits` (main.rs:9122) |
| network-transport | NT-06 Groww silent black-hole restart | scenario | `crates/app/src/groww_sidecar_supervisor.rs::should_restart_on_stall` / `test_should_restart_on_stall_true_when_stale` (FEED-STALL-01) |
| network-transport | NT-08 TLS handshake failure | error | `crates/core/src/websocket/tls.rs` (aws-lc-rs, no-ALPN, token-redacted url connection.rs:2267) |
| network-transport | NT-09 partial/truncated WS binary frame | exception | `crates/core/src/parser/full_packet.rs::test_parse_full_truncated` (full_packet.rs:268) |
| network-transport | NT-10 mid-frame abrupt TCP drop, WAL survives | scenario | `crates/core/tests/chaos_ws_e2e_wal_durability.rs::chaos_e2e_200_frames_land_in_wal_in_fifo_order` |
| network-transport | NT-11 subscription loss across reconnect | bug | `crates/core/src/websocket/connection.rs::SubscribeRxGuard` / `test_subscribe_rx_guard_reinstalls_on_drop` (connection.rs:5463) |
| network-transport | NT-12 reconnect storm short-session floor | condition | `crates/core/src/websocket/connection.rs::compute_short_session_reconnect_floor_ms` (connection.rs:2537) |
| network-transport | NT-13 ping/pong timeout | condition | `crates/core/src/websocket/activity_watchdog.rs` (50s threshold, server 40s close 806) |
| auth-token | AUTH-P01 JWT expires mid-session (807) | error | `crates/core/tests/gap_enforcement.rs::only_807_requires_token_refresh` + `mid_session_watchdog.rs` |
| auth-token | AUTH-P02 token force-renew on WS wake | scenario | `crates/core/src/auth/token_manager.rs::force_renewal_if_stale` (token_manager.rs:1062, AUTH-GAP-03) |
| auth-token | AUTH-P03 lane-owned TokenManager on cold-start | scenario | `crates/core/src/websocket/connection.rs::wake_renewal_token_manager` (connection.rs:615) |
| auth-token | AUTH-P04 dual-instance SET-NX lock | condition | `crates/core/src/instance_lock.rs::try_acquire_instance_lock` (SET-NX line 199, RESILIENCE-01) |
| auth-token | AUTH-P05 static-IP boot gate ordersAllowed=false | error | `crates/core/src/network/ip_verifier.rs::classify_static_ip_boot_outcome` (ip_verifier.rs:545) |
| auth-token | AUTH-P06 corrupt/wrong-account cached JWT | error | `crates/core/src/auth/token_cache.rs::load_token_cache` (token_cache.rs:147, AUTH-GAP-01) |
| auth-token | AUTH-P07 token-gen rate-limit (120s) | exception | `crates/core/src/auth/token_manager.rs::is_dhan_rate_limited` (token_manager.rs:1219) |
| auth-token | AUTH-P08 WS 429 cooldown vs fresh-token reconnect | condition | `crates/core/src/websocket/rate_limit_cooldown.rs` (WS-GAP-08) |
| auth-token | AUTH-P09 Groww access-token rejected | error | `crates/core/src/feed/groww/auth.rs::classify_groww_auth_failure` (auth.rs:367) |
| auth-token | AUTH-P10 Groww secret redaction in logs | scenario | `crates/core/src/feed/groww/auth.rs::test_parse_access_token_response_error_does_not_echo_input_bytes` (auth.rs:343) |
| auth-token | AUTH-P15 SSM unreachable at boot | error | `crates/core/src/instance_lock.rs::try_acquire_instance_lock` err→exit (main.rs:144) + token_manager transient backoff |
| questdb-persist-wal | QPW-01 QuestDB unreachable at boot (BOOT-01/02) | error | `crates/storage/src/boot_probe.rs::test_wait_for_questdb_ready_fails_on_unreachable_host` |
| questdb-persist-wal | QPW-02 QuestDB disappears mid-session, buffer | scenario | `crates/storage/tests/chaos_questdb_lifecycle.rs::questdb_disappears_ticks_buffer_not_drop` |
| questdb-persist-wal | QPW-03 WAL writer panic → supervised respawn | exception | `crates/storage/src/ws_frame_spill.rs` (WS-SPILL-01, catch_unwind respawn) |
| questdb-persist-wal | QPW-04 WAL writer dead at append → loud drop | bug | `crates/storage/src/ws_frame_spill.rs` Disconnected arm (WS-SPILL-02, tv_ticks_lost_total) |
| questdb-persist-wal | QPW-05 crash between replay & re-capture | scenario | `crates/app/tests/wal_replay_confirm_symmetry_guard.rs::confirm_replayed_called_from_both_boot_paths` |
| questdb-persist-wal | QPW-06 WAL replay FIFO ordering | scenario | `crates/storage/tests/ws_frame_order_preservation_guard.rs` + `chaos_ws_frame_wal_replay.rs` |
| questdb-persist-wal | QPW-07 WAL corrupted/truncated tail | error | `crates/storage/src/ws_frame_spill.rs::replay_segment` / `test_crc32_known_vector` |
| questdb-persist-wal | QPW-08 DEDUP collision via capture_seq | scenario | `crates/storage/tests/chaos_index_same_value_burst_preserved.rs` + `dedup_uniqueness_proptest.rs` |
| questdb-persist-wal | QPW-09 ticks table schema self-heal, no DROP | scenario | `crates/storage/src/tick_persistence.rs::ensure_tick_table_dedup_keys` (ticks_table_is_populated gate) |
| questdb-persist-wal | QPW-10 QuestDB probe exit damping | bug | `crates/app/src/main.rs::damp_questdb_exit_signal` / `test_damp_questdb_exit_signal_*` |
| questdb-persist-wal | QPW-11 IST-midnight seal burst 3-tier absorb | scenario | `crates/storage/tests/chaos_midnight_seal_burst.rs::test_midnight_burst_99k_fits_within_ring_cap_zero_drop` |
| questdb-persist-wal | QPW-12 mid-session disk-full WAL resilience | error | `crates/storage/src/ws_frame_spill.rs::test_writer_survives_unwritable_dir_then_recovers` (REFUTED gap → covered) |
| questdb-persist-wal | QPW-14 dedup feed-key uniqueness | scenario | `crates/storage/tests/dedup_segment_meta_guard.rs::per_feed_market_data_dedup_keys_must_include_feed` (REFUTED gap → covered) |
| questdb-persist-wal | QPW-15 QuestDB restart mid-write | scenario | `crates/storage/src/tick_persistence.rs::test_e2e_crash_recovery_zero_loss` (REFUTED gap → covered, default battery) |
| questdb-persist-wal | QPW-16 QuestDB down >300s Halt | condition | `crates/core/src/pipeline/no_tick_watchdog.rs` + SLO 10s loop + `chaos_questdb_full_session.rs` (REFUTED gap → covered) |
| questdb-persist-wal | QPW-17 WAL frame day-attribution IST midnight | out-of-box | `crates/storage/src/ws_frame_spill.rs::count_frames_for_ist_day` / `test_classify_frame_for_day_and_ist_day_of_wall_nanos` |
| feed-toggle-coldstart-races | FTC-01 Dhan disable refused while live | condition | `crates/api/src/handlers/feeds.rs::test_set_feed_dhan_disable_rejected_when_live_trading` (feeds.rs:544) |
| feed-toggle-coldstart-races | FTC-02 Dhan cold-start OFF→Running FSM | scenario | `crates/api/src/feed_state.rs::next_lane_state_off_to_starting_on_start` (feed_state.rs:765) |
| feed-toggle-coldstart-races | FTC-03 phantom-Running closed | bug | `crates/app/src/main.rs::phantom_running_closed_running_set_only_after_start_ok` (main.rs:9720) |
| feed-toggle-coldstart-races | FTC-04 Stopping→Starting rejected | condition | `crates/api/src/feed_state.rs::next_lane_state_rejects_stopping_to_starting` (feed_state.rs:856) |
| feed-toggle-coldstart-races | FTC-05 lane watchdog Halt no process::exit | bug | `crates/app/tests/post_market_pool_halt_guard.rs::runtime_lane_watchdog_does_not_process_exit` (main.rs:3884) |
| feed-toggle-coldstart-races | FTC-06 Groww runtime enable/disable | scenario | `crates/app/src/groww_sidecar_supervisor.rs::run_groww_sidecar_supervisor` (supervisor.rs:905) |
| feed-toggle-coldstart-races | FTC-07 both feeds ON cross-feed uniqueness | condition | `crates/storage/src/tick_persistence.rs::test_dedup_key_ticks_exact_format` (DEDUP feed-in-key) |
| feed-toggle-coldstart-races | FTC-08 feed alive-but-silent restart | bug | `crates/app/src/groww_sidecar_supervisor.rs::should_restart_on_stall` (FEED-STALL-01) |
| feed-toggle-coldstart-races | FTC-09 stall-restart storm ceiling | condition | `crates/app/src/groww_sidecar_supervisor.rs::StallRestartStorm::record_and_is_storm` (supervisor.rs:473) |
| feed-toggle-coldstart-races | FTC-10 feed supervisor respawn | exception | `crates/app/src/groww_sidecar_supervisor.rs::spawn_supervised_groww_sidecar_supervisor` (FEED-SUPERVISOR-01) |
| feed-toggle-coldstart-races | FTC-11 H5 gate re-assert before close | condition | `crates/app/src/main.rs::park_running_dhan_lane` / `gate_hold_reasserted_before_runtime_teardown` (main.rs:9737) |
| feed-toggle-coldstart-races | FTC-12 disable-during-cold-start CAS cancel | bug | `crates/app/src/main.rs::supervisor_cancel_is_cas_first_no_abort_of_a_promoted_running_lane` (main.rs:9671) |
| feed-toggle-coldstart-races | FTC-13 cold-start task dies → FSM recover | exception | `crates/app/src/main.rs::run_dhan_lane_runtime_supervisor` dead-task respawn (main.rs:8104) |
| feed-toggle-coldstart-races | FTC-16 boot-OFF enable, two watchers no double-spawn | condition | `crates/app/src/dhan_activation.rs::test_boot_off_toggle_on_does_not_falsely_mark_running` (dhan_activation.rs:403) |
| feed-toggle-coldstart-races | FTC-17 cold-start retry storm bounded + early-out | condition | `crates/app/src/main.rs::run_dhan_lane_cold_start` retry backoff (main.rs:8381) |
| feed-toggle-coldstart-races | FTC-18 lane teardown drain timeout (DHAN-LANE-04) | exception | `crates/app/src/main.rs::park_running_dhan_lane` timeout (main.rs:8534) |
| clock-calendar-longsleep | CCL-01 IST-midnight DayOhlc reset | scenario | `crates/trading/src/in_mem/day_ohlc_tracker.rs::test_tracker_reset_daily_disarms_all` |
| clock-calendar-longsleep | CCL-03 boot clock-skew >2s HALT (BOOT-03) | error | `crates/app/src/infra.rs::enforce_clock_skew_at_boot` / `test_clock_skew_threshold_constant_is_2s` |
| clock-calendar-longsleep | CCL-04 65h weekend dormant sleep | scenario | `crates/common/src/trading_calendar.rs::test_sleep_70h_friday_to_monday_then_connect_succeeds` |
| clock-calendar-longsleep | CCL-05 92h+ holiday-weekend sleep (calendar math) | scenario | `crates/common/src/trading_calendar.rs::test_next_trading_day_skips_consecutive_holidays_and_weekend` (full 92h dormant chaos still W6-2 outstanding) |
| clock-calendar-longsleep | CCL-07 late-tick re-fold vs discard | condition | `crates/trading/src/candles/aggregator_cell.rs` consume_tick (AGGREGATOR-LATE-01, Option B) |
| clock-calendar-longsleep | CCL-08 cross-day amend guard | condition | `crates/trading/src/candles/aggregator_cell.rs::force_seal` last_sealed empty (§4b) |
| clock-calendar-longsleep | CCL-09 boundary catch-up seal (BOUNDARY-01) | scenario | `crates/trading/src/candles/boundary_calc.rs::test_missed_boundaries_returns_four_for_5min_gap_m1` |
| clock-calendar-longsleep | CCL-10 holiday CSV thrash avoidance | scenario | `crates/storage/tests/aws_deploy_safety_guard.rs::holiday_gate_is_wired_and_fail_open` (REFUTED gap → covered at instance level) |
| clock-calendar-longsleep | CCL-11 09:15 volume/ring reset instant | condition | `crates/trading/src/in_mem/reset_scheduler.rs::test_reset_target_is_pinned_at_09_15_ist` |
| clock-calendar-longsleep | CCL-12 WS LTT no +19800 offset | bug | data-integrity.md rule + `test_critical_ws_timestamp_no_ist_offset_on_ts` |
| clock-calendar-longsleep | CCL-13 IST-midnight force-seal skips non-trading days | condition | `crates/app/src/main.rs` Task 3 is_trading_day guard (main.rs:4359) |
| clock-calendar-longsleep | CCL-14 no-DST fixed +19800 offset | out-of-box | `crates/common/src/trading_calendar.rs::ist_offset` / `test_ist_offset_returns_5h30m` |
| clock-calendar-longsleep | CCL-15 leap-second/reset-target boundary | exception | `crates/app/src/day_ohlc_orchestrator.rs` wait==0→full-day guard (day_ohlc_orchestrator.rs:104) |
| clock-calendar-longsleep | CCL-16 clock-skew probe unavailable → WARN not HALT | exception | `crates/app/src/infra.rs::probe_clock_skew` Unavailable path (main.rs:5214) |
| clock-calendar-longsleep | CCL-17 mid-session clock drift (candle bucketing) | out-of-box | L-H7 exchange-timestamp bucketing `aggregator_cell.rs` + `boundary_calc.rs::trading_date_ist` (REFUTED gap → covered/deferred residual) |
| boot-process-oom-restartloop | BP-01 post-READY crash-loop start-rate limit | scenario | `crates/storage/tests/aws_deploy_safety_guard.rs::test_systemd_unit_restart_policy` + market-hours-liveness-alarm.tf |
| boot-process-oom-restartloop | BP-02 app hangs at 08:30 boot | scenario | `deploy/aws/terraform/boot-heartbeat-alarm.tf` + `crates/app/tests/boot_completed_metric_guard.rs` |
| boot-process-oom-restartloop | BP-03 compose-plugin restart-loop self-heal | bug | `scripts/ensure-questdb.sh` (compose v2→v1→plugin→docker-run fallback) |
| boot-process-oom-restartloop | BP-04 wedged docker daemon hangs ExecStartPre | condition | `scripts/ensure-questdb.sh` dtimeout/ctimeout DOCKER_CALL_TIMEOUT_SECS=90 |
| boot-process-oom-restartloop | BP-05 post-09:10 market-hours liveness | scenario | `deploy/aws/terraform/market-hours-liveness-alarm.tf` (tv_realtime_guarantee_score MISSING→page) |
| boot-process-oom-restartloop | BP-06 09:10-09:20 no-page window | scenario | REFUTED — market-hours-liveness-alarm.tf + mem_used_high always-on cover the window (PR #1284) |
| boot-process-oom-restartloop | BP-10 EC2 fails to START at 08:30 | scenario | `deploy/aws/terraform/start-watchdog-lambda.tf` / `test_start_watchdog_lambda_monitors_the_morning_start` |
| boot-process-oom-restartloop | BP-11 EIP detach strands box | scenario | `variables.tf` enable_eip=true + `aws_deploy_safety_guard.rs::deploy_eip_is_enabled_by_default` + autopilot re-associate |
| boot-process-oom-restartloop | BP-12 disk-wedge fills 30GB gp3 | scenario | `deploy/aws/terraform/app-alarms.tf` disk_used_high + DISK-WATCHER-01 |
| boot-process-oom-restartloop | BP-15 fast-boot heartbeat-then-crash | condition | `deploy/aws/terraform/boot-heartbeat-alarm.tf` (eval 2×60s) + `boot_completed_metric_guard.rs` |
| boot-process-oom-restartloop | BP-16 boot umbrella deadline consistency | bug | `crates/common/tests/boot_timeout_consistency_guard.rs::token_init_timeout_does_not_exceed_umbrella_boot_timeout` |

---

## Real gaps matrix (ranked by severity, critical first)

| Dimension | Scenario | Failure mode | Proposed fix | Severity |
|---|---|---|---|---|
| auth-token | **AUTH-P12** runtime static-IP / EIP change mid-session | `crates/core/src/network/ip_monitor.rs::spawn_ip_monitor` has ZERO production call site (only its own `#[cfg(test)]` callers + `// TEST-EXEMPT`); `main.rs` wires ONLY boot-time `verify_public_ip`/`verify_static_ip_at_boot`. A mid-session public-IP/EIP change is never re-verified at runtime. `guarantees.md:118` falsely cites the never-spawned detector as proof. | Wire `spawn_ip_monitor` into `main.rs` boot (expected EIP from config/SSM) so `IpCheckResult::Mismatch` fires `GapNetIpMonitor` CRITICAL + halt; add a pub-fn-wiring-guard entry so it cannot become dead code; optionally re-run `verify_static_ip_at_boot` hourly. | high |
| auth-token | **AUTH-P13** EIP fully detaches → no public route | Even wired, `ip_monitor` depends on an external IP-echo fetch. With no route, both fetches fail → `IpCheckResult::CheckFailed` logged at `warn!` ("transient, retry next interval", ip_monitor.rs:181) and NEVER escalates. `mid_session_watchdog` classifies "network unreachable"/"no route to host" as transient (mod.rs) → `tv_mid_session_profile_transient_failure_total` with NO alert rule. Stranded box indistinguishable from a benign blip; also breaks SSM renewal + Dhan feed silently. | Bounded-consecutive-CheckFailed escalation: after N consecutive `CheckFailed` (both endpoints) in market hours emit a CRITICAL reachability ErrorCode; cross-signal AWS IMDS `169.254.169.254` public-ipv4 to distinguish "no public IP assigned" from "echo service down"; page to re-associate EIP. | medium |
| auth-token | **AUTH-P11** TOTP secret rotated externally (AUTH-GAP-04) | AUTH-GAP-04 is a RESERVED stub only (crossref allowlist `error_code_rule_file_crossref.rs:65`) — NO `AuthGap04` ErrorCode variant, NO emit site, NO detection. `token_manager.rs::is_totp_error` retries `TOTP_MAX_RETRIES` then returns a generic `AuthenticationFailed`. Mitigating (severity-lowering): the generic alert's reason already points at the exact SSM TOTP secret param. Missing: distinct typed code/named alert/audit. | Promote AUTH-GAP-04 → `AuthGap04TotpRotatedExternally` (Critical), emit from the TOTP-exhaustion terminal branch with operator-action "verify `/tickvault/<env>/dhan/totp-secret` vs dhan.co", remove from crossref allowlist; add a pre-market TOTP-vs-token-gen self-check at 08:45. | medium |
| auth-token | **AUTH-P14** Groww daily-token expires mid-session | Verdict: **REFUTED → not a surviving gap.** FEED-STALL-01 kills+relaunches the sidecar on data-silence; relaunch mints a fresh (non-persisted) token. Residual = ~30s detection latency only. | (covered) | — |
| feed-toggle-coldstart-races | **FTC-14** WS-pool cold-start failure mislabeled DHAN-LANE-01 | `classify_start_lane_error` (main.rs:7944) maps `BootAbortClean→DhanLane03`, `BootAbortErr→DhanLane01`; NO path to `DhanLane02WsPoolSpawnFailed` (ZERO emit sites). A runtime WS-pool spawn/handshake failure points the operator at the wrong (`universe_build_failed`) runbook. Cross-ref ratchet only requires the variant be MENTIONED, not emitted → invisible. | Split `StartLaneError` to carry the failing stage (add `WsPoolSpawn(anyhow::Error)` or thread a stage tag through `BootAbortErr`), tag the pool-create/spawn path so `classify_start_lane_error` emits `DhanLane02` with `stage='ws_pool'`; add a unit test asserting a WS-pool-stage error classifies to DhanLane02. | medium |
| clock-calendar-longsleep | **CCL-06** Muhurat session ticks silently dropped | `main.rs:5617-5618` sets `should_connect_ws = ... \|\| is_muhurat` (feed connects), but `tick_processor.rs::is_within_persist_window` (line 119) is hardcoded `[09:00, 15:30)` with NO muhurat branch; the word "muhurat" appears nowhere in `tick_processor.rs`. An 18:15 IST tick fails the range → `continue` at line 1163 → dropped before persist/seal/broadcast. Whole ~1h Diwali muhurat session stores ZERO data despite a live connection (audit Rule 11 false-OK). `boundary_calc.rs:11` self-admits "L-C4 Muhurat deferred to Wave 7". | Thread `is_muhurat` into the persist-window check (config-driven `muhurat_open`/`muhurat_close` or a `MUHURAT_*_SECS` const set); add ratchet `test_muhurat_tick_1815_ist_is_persisted`. Alternatively, if muhurat capture is intentionally out of scope, remove `is_muhurat` from `should_connect` and document it — the current state is the worst of both. | high |
| clock-calendar-longsleep | **CCL-02** DayOhlc reset task unsupervised + runbook drift | `spawn_midnight_reset_task` (day_ohlc_orchestrator.rs:94) is a bare `tokio::spawn` loop with NO catch_unwind, NO respawn, NO failure counter. Runbook INDEX-OHLC-02 cites `tv_day_ohlc_reset_failures_total` (grep = zero hits) and `ErrorCode::IndexOhlc02DailyResetFailed` exists but has ZERO emit site. (parking_lot Mutex does not poison → the specific panic vector is near-unreachable, hence medium.) | Wrap `reset_daily_all()` in a supervised respawn wrapper (mirror WS-GAP-05 / DISK-WATCHER-01), increment `tv_day_ohlc_reset_failures_total`, emit `ErrorCode::IndexOhlc02` on failure; add a source-scan ratchet asserting the counter name exists so the runbook stops lying. | medium |
| questdb-persist-wal | **QPW-13** disk-full pre-flight gate + STORAGE-GAP-05 code absent | NO free-bytes reject/abort GATE at the spill/DLQ append boundary (the df check in `tick_persistence.rs::open_spill_file` is alert-only, one-shot on lazy first-open); NO `StorageGap05` ErrorCode variant/emit site (Reserved only). Data-loss HARM is already covered by the write-failure→DLQ path (`spill_tick_to_disk_seq` + `chaos_disk_full.rs` assert ticks_dropped==0) → severity lowered. | Optional: cached free-bytes pre-flight (statvfs into the existing 60s watcher's AtomicU64) consulted by seal/tick/ws_frame spill append; escalate one tier earlier when free<threshold; wire the reserved `StorageGap05` code to the existing low-disk error! site + ratchet. | low |
| network-transport | **NT-15** per-SID cold black-hole (never-first-ticks SID) | `TickGapDetector::record_tick` (tick_gap_detector.rs:84) only inserts on a real parsed tick; scan_gaps iterates only the map → a never-ticked SID never becomes a key, never contributes to `tv_tick_gap_instruments_silent`. `freshest_tick_age_secs` returns MIN → one cold SID masked while any other SID ticks. Universe-wide cold black-hole IS caught (aggregator-no-seals + SLO); per-SID is invisible. No `expected_subscribed MINUS ticked` reconciliation exists. | Seed `TickGapDetector.last_seen` at subscribe time (record every subscribed `(security_id, segment)` with the subscribe instant baseline) so scan_gaps flags a never-ticked SID after `threshold_secs` of market hours — turns WS-GAP-06 + `tv_tick_gap_instruments_silent` into a first-tick black-hole detector at zero extra infra. | medium |
| network-transport | **NT-05** Dhan data-silence has NO forced reconnect (FEED-STALL-01 parity) | Detection/paging IS present and ping-immune (`no_tick_watchdog.rs` CRITICAL@120s in both boot paths + WS-GAP-06 + FeedHealthRegistry), so the "silent" premise was refuted. Residual: unlike Groww FEED-STALL-01 (which restarts the sidecar), Dhan only detects+alerts on data-silence — it does not force a reconnect purely on data-silence. Self-healing-automation delta, not a detection gap. | Rescope narrowly: add a data-silence-triggered forced Dhan reconnect (FEED-STALL-01 parity) — a separate data-frame-only counter (Binary-only, never ping/pong) driving a market-hours-gated reconnect. | low |
| boot-process-oom-restartloop | **BP-09** autopilot cannot recover a systemd `failed` crash-loop | `aws-autopilot.sh` runs `systemctl restart` on a StartLimit-`failed` unit, but restart returns "Start request repeated too quickly" and does NOT clear the start-limit counter — systemd needs `reset-failed` FIRST (never invoked; only appears as doc in tickvault.service). Autopilot's restart is a no-op: it correctly pages but cannot recover; box stays down until a human runs reset-failed. `aws-tickvault-exit-loop.md` says "3 retries" while StartLimitBurst=8 (doc drift). | Before restart, detect `systemctl is-failed tickvault` and run `systemctl reset-failed tickvault` then `systemctl start`; fix the runbook to "8 restarts in 10 min"; add a guard test asserting autopilot contains `reset-failed`. | high |
| boot-process-oom-restartloop | **BP-07** OOM kill (PROC-01) has no dedicated signal | `wave-4-error-codes.md` PROC-01 is "Reserved" — NO `Proc01` ErrorCode variant, NO `crates/app/src/oom_monitor.rs` (the runbook names a non-existent file). An OOM is only caught indirectly (die → systemd → market-hours-liveness page on missing SLO); no OOM attribution; OOM-loop indistinguishable from panic-loop. | Implement the PROC-01 OOM monitor: tokio task reading cgroup-v2 `memory.events` `oom_kill` vs boot baseline → `error!(code=PROC-01)` + `tv_oom_kills_total` + CloudWatch alarm; add the `Proc01` ErrorCode variant + runbook cross-ref. | medium |
| boot-process-oom-restartloop | **BP-08** fd/RSS/spill early-warning (RESOURCE-01/02/03) | RESOURCE-01/02/03 are all "Reserved" — no ErrorCode variants, no emit sites. Host-level `mem_used_high`/`disk_used_high` exist, but there is NO fd-count monitor at all (LimitNOFILE=65536 can be exhausted by leaked WS/QuestDB sockets with zero signal until connect() fails), and no process-level RSS-vs-cgroup or spill-vs-free early alarm distinct from host aggregate. | fd-count sampler (`/proc/self/fd` vs LimitNOFILE) → `tv_open_fds` + RESOURCE-01 @80%; wire `subsystem_memory` RSS → RESOURCE-02; expose DISK-WATCHER free-bytes as percent-of-free → RESOURCE-03; promote the three codes to real ErrorCode variants. | medium |
| boot-process-oom-restartloop | **BP-14** EC2 System Status Check has no auto-recover action | `alarms.tf` `system_status_check` pages on `StatusCheckFailed_System` but has NO `arn:aws:automate:<region>:ec2:recover`/`:reboot` action. A hardware fault during market hours pages but the box does not self-migrate; near 16:30 stop the alarm may flip notBreaching-OK and be forgotten. Autopilot only handles a cleanly-stopped box, not a status-impaired running one. | Add `arn:aws:automate:${region}:ec2:recover` (System check) + `:reboot` (hung Instance check) alarm actions alongside SNS; add a ratchet asserting the recover action is present. | medium |
| boot-process-oom-restartloop | **BP-17** NET-01/NET-02 mid-session IP-change / DNS-cascade unwired | NET-01 (IP changed mid-session, IMDS poll) + NET-02 (DNS cascade) are "Reserved" — no ErrorCode variants. `ip_monitor.rs`/`ip_verifier.rs` exist as pure helpers but the detection loops are not wired to typed codes/alarms. `dry_run=true` today (order-rejection dormant) → low now, but MUST be live before real trading. | Wire the IMDS-poll loop → `error!(code=NET-01)` + `tv_ip_changed_total` on IP change; NET-02 on 3 consecutive DNS failures across the 4 Dhan hosts; promote both to real ErrorCode variants + CloudWatch alarms; gate as a pre-live-trading requirement. | low |
| boot-process-oom-restartloop | **BP-13** deploy binary integrity (parallel-build disk-wedge) | `deploy-aws.yml` enforces a 30MB SIZE cap but NO sha256/artifact-integrity check of the deployed binary vs the CI artifact, and no build-host disk pre-flight. A parallel-build disk-wedge producing a truncated (<30MB but corrupt) binary passes the cap, ships via SSM, and crash-loops. The exit-loop runbook lists "corrupt binary" as cause #2 with a manual sha256 step nothing enforces. | Add a sha256 manifest to the deploy artifact; SSM deploy step verifies the copied binary's sha256 vs CI value BEFORE `systemctl restart` (fail + page on mismatch, keep old binary); add a build-host `df` pre-flight. | low |
| feed-toggle-coldstart-races | **FTC-15** StopAborted FSM transition is dead code | `LaneEvent::StopAborted` / `next_lane_state(Stopping, StopAborted)->Running` (feed_state.rs:145) is fully defined + unit-tested but has ZERO production emit site. `park_running_dhan_lane` re-checks the gate only BEFORE Running→Stopping, then teardown + StopJoined→Off unconditionally (main.rs:8556) — never re-checks once inside Stopping. The runbook FSM diagram + doc comment advertise a recovery the runtime never exercises (Rule 13/14 dead-code-with-tests). Low: H5 pre-Stopping check + no-orders dry_run contract shrink the exposure. | Either (a) wire StopAborted: re-check `can_disable_dhan()` one more time before the irreversible WS-close inside teardown and on gate-closed call `advance_dhan_lane(StopAborted)` → Running + abort teardown; or (b) remove the StopAborted variant+transition and document H5 as single-checkpoint. Option (a) preferred (closes the TOCTOU the main.rs:8486 comment admits it only "narrows"). | low |
| auth-token | **AUTH-P16** instance-lock stale-takeover TOCTOU | Verdict: **REFUTED → not a surviving coverage gap.** RESILIENCE-01 heartbeat ownership check detects the dual-instance window ≤30s and fail-closed HALTs; wired across ErrorCode + heartbeat + boot-halt + triage + runbook. Residual = optional CAS-atomic stale takeover hardening only. | (covered; optional hardening) | low |

---

## Fix queue — critical + high real gaps as serial PRs

The following survived refutation at **high** severity (no critical survived refutation). Open
these as serial PRs per the PR-completion protocol (one open at a time, merge before next).
All are pre-live-trading requirements; `dry_run=true` keeps them off the critical-financial path
today, but each is a real observability/recovery hole.

1. **PR-1 — AUTH-P12: wire the runtime static-IP / EIP-change monitor.**
   Spawn `crates/core/src/network/ip_monitor.rs::spawn_ip_monitor` from `crates/app/src/main.rs`
   boot (expected EIP from config/SSM), route `IpCheckResult::Mismatch` to the existing
   `GapNetIpMonitor` CRITICAL + halt, and add a pub-fn-wiring-guard entry so it can never regress
   to dead code. Fixes the false coverage claim in `docs/architecture/guarantees.md:118`.

2. **PR-2 — BP-09: make aws-autopilot actually recover a `failed` crash-loop.**
   In `scripts/aws-autopilot.sh`, detect `systemctl is-failed tickvault`, run
   `systemctl reset-failed` then `systemctl start` (not bare `restart`); correct
   `docs/runbooks/aws-tickvault-exit-loop.md` to "8 restarts in 10 min"; add a guard test
   asserting autopilot invokes `reset-failed`.

3. **PR-3 — CCL-06: stop silently dropping Muhurat-session ticks.**
   Thread `is_muhurat` (already computed in `main.rs`) into
   `crates/core/src/pipeline/tick_processor.rs::is_within_persist_window` /
   `is_wall_clock_within_persist_window` (config-driven `muhurat_open`/`muhurat_close` or a
   `MUHURAT_*_SECS` const set); add ratchet `test_muhurat_tick_1815_ist_is_persisted`. If muhurat
   capture is intentionally out of scope, instead remove `is_muhurat` from `should_connect_ws` and
   document it — the current connect-and-drop-everything state is the worst option.

Then sweep the **medium** tier (AUTH-P11 TOTP-rotation code, AUTH-P13 stranded-box escalation,
FTC-14 DHAN-LANE-02 stage tagging, CCL-02 DayOhlc reset supervisor, NT-15 per-SID cold
black-hole seeding, BP-07 PROC-01 OOM monitor, BP-08 RESOURCE-01/02/03, BP-14 EC2 auto-recover)
as follow-on serial PRs before flipping `dry_run = false`.
