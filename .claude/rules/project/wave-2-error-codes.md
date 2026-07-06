# Wave 2 Error Codes

> **Authority:** This file is the runbook target for the new ErrorCode
> variants added in the Wave 2 hardening implementation. The cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## WS-GAP-04 — main-feed WebSocket entered post-close sleep

**Trigger:** the main-feed (or depth-20 / depth-200) WebSocket reached the
post-close gate (≥15:30 IST + ≥3 consecutive failures) and is now sleeping
until the next market open instead of giving up. Replaces the legacy
`return false` give-up path that required a process restart.

**Triage:**
1. This is informational (Severity::Low). The connection is dormant, not
   failed.
2. Verify `tv_ws_post_close_sleep_total{feed="main"}` increments in
   Prometheus.
3. At next market-open IST, expect a corresponding
   `WebSocketSleepResumed` event followed by normal connect.

**Source:** `crates/core/src/websocket/connection.rs::wait_with_backoff`

## WS-GAP-05 — pool supervisor respawned a dead connection task

**Trigger:** `respawn_dead_connections_loop` in `connection_pool.rs`
detected a pool-task `JoinHandle` that exited and respawned it within 5s.
Indicates a panic / unexpected return inside the per-connection task.

**Triage:**
1. Search `data/logs/errors.jsonl.*` for the panic backtrace immediately
   preceding the respawn.
2. Repeated respawns (`tv_ws_pool_respawn_total` rate >1/min for 5min)
   indicate cascading failure — operator must inspect.

**Source:** `crates/core/src/websocket/connection_pool.rs::spawn_pool_supervisor_task`

## WS-GAP-06 — tick-gap detector fired a coalesced summary

**Trigger:** Item 8's `TickGapDetector` observed ≥1 instrument with
silence ≥30s during the most recent 60s coalesce window.

**Triage:**
1. Inspect `data/logs/errors.jsonl.*` for the `TickGapsSummary` event;
   it carries a `top_10_samples` list `(symbol, gap_secs)`.
2. If gap symbols cluster on one segment, check for a Dhan-side
   ExchangeSegment outage.
3. If gap symbols are scattered, check `tv_websocket_connections_active`
   — a slow socket can starve a subset of instruments.

**Source:** `crates/core/src/pipeline/tick_gap_detector.rs::TickGapDetector::scan`

## WS-GAP-07 — live-feed frame channel closed (tick consumer died)

**Trigger:** the main-feed read loop's `frame_sender.try_send(frame)`
returned `TrySendError::Closed` — the downstream tick-processing consumer
(`run_tick_processor`) holding the `Receiver` has been dropped. The read
loop logs `error!` (code `WS-GAP-07`), increments
`tv_ws_live_channel_closed_drop_total{ws_type="live_feed"}`, and returns,
stopping frame forwarding on that connection. Severity::High.

**Why High (not Low like the post-close sleep):** unlike a backpressure
`Full` (the WAL still durably records the frame), a `Closed` channel means
the consumer task is GONE — no ticks reach the pipeline from this
connection until the consumer/app restarts. The previous code logged this
at `warn!` with no counter, so the operator was blind to a dead consumer
mid-market (audit Rule 5 — drain failures must be `error!`).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — search for `WS-GAP-07`; the event
   carries `connection_id`.
2. Check `tv_ws_live_channel_closed_drop_total` rate — a non-zero value
   during market hours means the tick processor died. Look in
   `data/logs/errors.jsonl.*` for a panic backtrace from
   `run_tick_processor` immediately preceding the WS-GAP-07 line.
3. The pool supervisor (WS-GAP-05) respawns the *connection* task, but it
   cannot resurrect a dead *consumer*. If the consumer is gone, restart the
   app — boot re-creates the channel + consumer.

**Source:** `crates/core/src/websocket/connection.rs` (the
`TrySendError::Closed` arm of the live-feed `try_send`).

## WS-GAP-08 — Dhan 429 rate-limit cooldown persisted across a process restart

**Trigger:** the main-feed WebSocket connect was rate-limited by Dhan with
HTTP 429 (DATA-805 "too many requests/connections" class). The in-memory
`rate_limit_streak` 60s→120s→240s→300s reconnect floor already absorbs this
WITHIN a running process — but when every connection has been down for
`POOL_HALT_SECS` (300s) the pool watchdog returns Halt and the BOOT-ON path
calls `std::process::exit(2)` so a supervisor restarts the process. That
restart wipes the in-memory streak to `0`, and the fresh process reconnects
with a `0ms` first retry (`compute_reconnect_base_delay_ms(0) == 0`) straight
back into Dhan's still-active 429 window → instant 429 → 300s → exit →
restart → **infinite loop**.

This code tags two events from the persisted-cooldown fix
(`crates/core/src/websocket/rate_limit_cooldown.rs`):

1. **Boot-time wait (the common, healthy case)** — at boot, BEFORE the first
   Dhan WS connect, the app reads the persisted cooldown
   (`./data/ws-rate-limit-cooldown.json`, sibling of the WS-frame WAL). If a
   cooldown is still active, the app `info!`s with `code = WS-GAP-08`,
   increments `tv_ws_rate_limit_cooldown_waited_total`, and
   `tokio::time::sleep`s out the remaining time (capped at
   `WS_RATE_LIMIT_BACKOFF_CAP_MS` = 300s) so it does NOT reconnect into the
   still-active 429 window. This is the fix that breaks the loop.
2. **Persist-write failure (rare)** — `record_rate_limit_hit` could not write
   the advisory file (disk full / read-only). Logged at `error!` with
   `code = WS-GAP-08`. Best-effort only: the in-memory streak still applies
   for the running process; only the cross-restart protection is lost until
   the disk recovers.

**Severity:** Low. The cooldown is an advisory, **fail-open** protection: a
missing / corrupt / stale file NEVER blocks boot, and the wait is bounded by
the cap so a bad file can never hang boot beyond 5 minutes. It does NOT
change the 429 backoff math (that is correct) and does NOT touch the
reconnect engine.

**Triage:**
1. Seeing the boot-time wait `info!` (event 1) is the system working as
   intended — it is waiting out a real Dhan rate-limit before reconnecting.
   If it recurs every boot, Dhan is sustainedly rate-limiting this account:
   check `tv_ws_rate_limit_cooldown_waited_total` rate and the connection
   count (max 5 WS/account); cross-check DATA-805 in
   `mcp__tickvault-logs__tail_errors`.
2. An `error!` (event 2) means the advisory file could not be written:
   `df -h data/` + `ls -la data/` for disk-full / permission / mount issues.
   The running process is still protected by its in-memory streak; the gap
   is only across a restart until the disk is fixed.

**Auto-triage safe:** YES (Severity::Low; the wait is self-bounded and the
file is fail-open).

**Source:** `crates/core/src/websocket/rate_limit_cooldown.rs`
(`remaining_cooldown_ms` / `read_cooldown` / `record_rate_limit_hit`),
the 429 classification write site in
`crates/core/src/websocket/connection.rs`, the boot read+wait sites in
`crates/app/src/main.rs` (`start_dhan_lane` slow lane + the FAST
crash-recovery boot arm — see the 2026-07-06 update below),
`crates/common/src/error_code.rs::WsGap08RateLimitCooldown`.

### 2026-07-06 Update — FAST crash-recovery boot arm now waits too (audit gap closed)

**The gap (audit-confirmed HIGH, 2026-07-06):** the boot-time wait above
(`wait_out_persisted_ws_rate_limit_cooldown`) had exactly ONE call site —
the SLOW lane (`start_dhan_lane`, gated on `should_connect_ws`). The FAST
crash-recovery boot arm (market-hours restart with a valid cached token)
created + spawned its WS pool with NO cooldown read. A mid-market
`process::exit(2)` — still reachable via the WS-GAP-09 `ceiling_exceeded`
fallback — with a valid cached JWT (a 429 does NOT invalidate the token)
routes through FAST BOOT, wipes the in-memory `rate_limit_streak`, and
reconnects with a 0ms first retry straight back into Dhan's still-active
429 window: the exact instant-429 restart loop this code exists to break,
just through the other boot door.

**The fix:** the fast arm now calls the SAME
`wait_out_persisted_ws_rate_limit_cooldown().await` immediately BEFORE its
`create_websocket_pool` call, mirroring the slow-lane invocation. Semantics
unchanged: fail-open on a missing/corrupt/stale file (no wait), bounded by
`WS_RATE_LIMIT_BACKOFF_CAP_MS` (5 min). Honest envelope: the fast lane's
crash-recovery boot is delayed ONLY when a real persisted 429 cooldown is
active — and then waiting is exactly the desired behaviour (reconnecting
instantly would earn the next 429); worst case is the 5-minute cap.

**Ratchet:** `crates/app/tests/ws_rate_limit_cooldown_wiring_guard.rs` —
source-order scan (house pattern of
`ratchet_tick_processor_spawns_before_reinject_await`) pinning that BOTH
`create_websocket_pool(` call sites in main.rs are preceded by their own
cooldown wait, plus a stub-guard that the wait keeps reading the persisted
file and stays clamped to the cap.

## WS-GAP-09 — pool watchdog reconnected IN PLACE instead of `process::exit`

**Trigger:** the pool watchdog reached its `>300s all-down → Halt` verdict, but
the down-cause classified as a **benign bare-Dhan-transport-RST** class rather
than a genuine fatal. Dhan was observed (CloudWatch, 2026-06-30) to silently
TCP-RST the main-feed socket ~5-6s after each connect. The legacy behaviour was
`std::process::exit(2)` → supervisor restart → a full 775-SID cold re-subscribe,
which trips Dhan's per-IP HTTP 429 (the system restarted ~27×/hr). Fix A
(2026-06-30) makes that exit CONDITIONAL: the watchdog consults a pure
`is_bare_reset_class(healths, token_valid, questdb_reachable)` that is true ONLY
when ALL hold — every connection's `rate_limit_streak == 0` (not a real 429), no
connection saw a `NonReconnectableDisconnect`, the token is valid, AND QuestDB is
reachable. When true (and inside the ceiling, below), the watchdog does NOT exit:
it `pool.reset_watchdog()`s (restarts the 300s `AllDown` window), keeps the
per-connection `wait_with_backoff` reconnect loops running (subscriptions
preserved by `SubscribeRxGuard`), and emits this code.

### 2026-06-30 extension — in-window Dhan 429 also rides out in place

The original Fix A above deliberately refused a `rate_limit_streak > 0` (a real
Dhan HTTP 429): `is_bare_reset_class` requires every connection's streak == 0.
But that left the audit's HIGH finding open — an in-MARKET-HOURS 429 still
classified GENUINE-FATAL → `process::exit(2)` → a 775-SID cold re-subscribe →
another 429 → loop (observed 2026-06-30: 61 main-feed 429s in one market window;
the restart loop also 429-starved the independent Groww feed's auth). A 429 is
the EXACT case the per-connection reconnect loops already absorb IN PLACE — they
wait out the `compute_rate_limit_floor_ms` 60s→5m floor (WS-GAP-08) and retry,
subscriptions preserved by `SubscribeRxGuard`; exiting on it is the
self-inflicted harm. So the ride-out decision is widened: the watchdog now
consults `should_reconnect_in_place(healths, in_market_hours, token_valid,
questdb_reachable)` = `is_bare_reset_class(...)` OR a NEW sibling
`is_in_window_429_rideout_class(...)` that admits a 429 ONLY when ALL hold —
`in_market_hours` (09:00–15:30 IST), token valid, QuestDB reachable, NO
connection saw a `NonReconnectableDisconnect`, and at least one connection has
`rate_limit_streak > 0`. Both classes share the SAME genuine-fatal guards and
the SAME 15-min ceiling, so a dead token / dead DB / non-reconnectable code
still exits, and a truly-stuck 429 still falls back to the genuine-fatal restart
after 15 min — never WORSE than today.

This code tags three events:

1. **Benign in-place decision** (`reason="bare_dhan_reset"`) — the common,
   healthy case: a streak==0 bare-RST storm with a valid token + reachable
   QuestDB is ridden out by reconnecting in place, no process restart, no 429.
2. **In-window 429 ride-out** (`reason="in_window_429_ride_out"`) — the
   2026-06-30 extension: an in-MARKET-HOURS Dhan 429 (streak>0) with a valid
   token + reachable QuestDB + no non-reconnectable code is ridden out in place
   (the per-connection loops honor the WS-GAP-08 429 cooldown floor), instead of
   restarting + a cold re-subscribe that just earns the next 429. This is the
   fix for the self-inflicted restart/429 loop.
3. **Ceiling-exceeded fallback** (`reason="ceiling_exceeded"`) — the
   second-tier safety valve (covers BOTH ride-out classes): if reconnect-in-place
   persists past `POOL_RECONNECT_IN_PLACE_CEILING_SECS` (= 900s = **15 minutes**)
   with still zero frame recovery, the watchdog falls back to the genuine-fatal
   `process::exit(2)` (or lane teardown). So the worst case degrades to today's
   behaviour, just after 15 min instead of 5 — strictly never worse.

**Severity:** Low. The benign in-place case is the system self-healing without
operator action; it does NOT page Telegram (the genuine-fatal Halt keeps its
existing `WebSocketPoolHalt` page). The `tv_pool_self_halts_total` counter now
increments ONLY on a genuine-fatal Halt, so its meaning sharpens — a non-zero
rate there is a real restart, not a benign reset ride-out.

**Honest envelope / operator note:** the 15-minute ceiling is a **second-tier
backstop flagged for operator sign-off** — it bounds a worst-case
truly-wedged-feed outage to 15 min (vs today's 5 min) before restarting. It does
NOT stop Dhan's server-side resets (a Dhan support ticket tracks that); it stops
the SELF-INFLICTED restart/429 storm those resets were triggering.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `WS-GAP-09`; the `reason` field is
   `bare_dhan_reset` (streak==0 RST ride-out), `in_window_429_ride_out` (a real
   in-market Dhan 429 ridden out in place — the 2026-06-30 fix), or
   `ceiling_exceeded` (fell back to exit).
2. `tv_ws_watchdog_reconnect_in_place_total{reason}` rate — a sustained
   `bare_dhan_reset` rate during market hours means Dhan is RST-storming; a
   sustained `in_window_429_ride_out` rate means Dhan is 429-storming (check the
   account's per-IP rate limit + the locked 1 main-feed conn). In BOTH cases the
   system is ABSORBING it in place instead of self-restarting. A `ceiling_exceeded`
   increment means a 15-min reconnect-in-place did NOT recover frames → the
   process restarted; treat like the legacy Halt and cross-check token / QuestDB /
   Dhan status.
3. Confirm the feed recovered: `tv_websocket_connections_active` should return to
   the full count once Dhan stops resetting / rate-limiting.

**Auto-triage safe:** YES (Severity::Low; the benign cases already self-healed,
and the ceiling fallback restarts exactly as the legacy Halt did).

**Source:** `crates/app/src/main.rs` (`is_bare_reset_class` +
`is_in_window_429_rideout_class` + `should_reconnect_in_place` +
`reconnect_in_place_ceiling_exceeded` pure helpers + the conditional Halt arm of
`spawn_pool_watchdog_task`), `crates/core/src/websocket/types.rs`
(`ConnectionHealth::{rate_limit_streak, saw_non_reconnectable}`),
`crates/common/src/error_code.rs::WsGap09WatchdogReconnectInPlace`.

## WS-GAP-10 — order-update WebSocket in-market outage (in-loop HIGH page)

**Trigger:** ≥ `ORDER_UPDATE_OUTAGE_PAGE_FAILURE_THRESHOLD` (3) consecutive
in-market order-update reconnect failures → ONE
`error!(code = "WS-GAP-10", reason = "in_market_outage")` + ONE `[HIGH]`
`OrderUpdateDisconnected` Telegram per outage episode. The per-episode latch
re-arms ONLY after a reconnect survives
`ORDER_UPDATE_RECONNECT_STABILITY_SECS` (60s); the WS-GAP-04 post-close sleep
and a PR-E Dhan re-enable also start a fresh episode. Every 10th failure
re-logs `error!(reason = "threshold_streak")` — CloudWatch-visible, never a
re-page. An outage whose streak accumulated off-hours pages on the FIRST
in-market failure at/above the threshold (the `>=` latch). A third tagged
event, `reason = "task_exited_unreachable"`, is the defensive log at the
main.rs order-update spawn site — `run_order_update_connection` is an
infinite never-give-up loop and structurally cannot return; if that line ever
executes, a future refactor broke the loop contract.

**Why it exists (2026-07-06 incident):** 14:05:49 IST — dead token → Dhan
TCP-RST ~10ms after the MsgCode-42 login, 39+ consecutive failures at ~1/min;
the operator saw ONLY false `[LOW] OrderUpdateReconnected x8`-style batches
because the recovery event fired on the CONNECT edge (10ms before each socket
died) and the only High emit site was dead code behind a never-returning
function (the WS-GAP-04 rewrite removed the legacy `return`).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `WS-GAP-10`; the payload carries
   `consecutive_failures` + `cause` (the classified disconnect source).
2. Dead-token signature: "Connection reset without closing handshake" ~10ms
   after login + DH-901/"Invalid Token" from the 15-min profile watchdog →
   restart / force re-auth (token re-mint automation is a LATER PR).
3. Cross-check the `tv-prod-order-update-ws-inactive` CloudWatch alarm +
   `tv_order_update_ws_active` gauge.
4. Counters: `tv_order_update_outage_pages_total` (one per HIGH page) /
   `tv_order_update_stable_reconnects_total` (one per survived recovery).

**Honest envelope:** the recovery Telegram means "survived 60s connected",
NOT "an order frame arrived" — the order-update stream is legitimately
silent for hours on idle days (watchdog threshold history
660s → 1800s → 14400s), so first-frame gating would never fire; time-survival
is the honest gate, with the 14400s activity watchdog as the dead-socket
backstop. Clean-close flaps (server Close frame / stream end) that never
error keep the legacy silent streak-reset (documented gap; a clean-close
streak detector is a PR-2 candidate) — EXCEPT mid-paged-episode, where the
streak + latch are kept. A pathological connect → survive-60s → die metronome
re-pages at most ~1 HIGH per ~2 min — bounded, and each cycle is a genuine
outage + recovery. An outage shorter than 3 failures pages nothing (absorbed
by the 0/0/500ms backoff ladder).

**Auto-triage safe:** YES (the loop self-retries; triage inspects — the page
is the visibility, not an action request).

**Source:** `crates/core/src/websocket/order_update_connection.rs::{run_order_update_connection, should_page_outage, streak_after_clean_close, should_emit_stable_recovery}`
(`ErrorCode::WsGap10OrderUpdateOutage`); defensive unreachable-return log in
`crates/app/src/main.rs` (order-update spawn site).

## AUTH-GAP-03 — token force-renewed on WebSocket wake

**Trigger:** Item 5's wake-from-sleep path observed
`TokenManager::next_renewal_at()` was within 4h of expiry and called
`force_renewal()` BEFORE attempting the post-sleep reconnect.

**Triage:**
1. This is informational (Severity::Low). It prevents the legacy
   "wake-up with stale token → DH-901 → halt" path.
2. Verify `tv_token_force_renewal_total{trigger="ws_wake"}` increments
   in Prometheus.

**Source:** `crates/core/src/auth/token_manager.rs::TokenManager::force_renewal_if_stale`

## BOOT-01 — slow-boot QuestDB readiness deadline approaching

**Trigger:** Item 7's `wait_for_questdb_ready` exceeded 30 seconds before
QuestDB confirmed readiness. Severity::High. The rescue ring is buffering
ticks; emergency action not yet required, but operator should investigate
Docker health (`docker ps` + `make doctor`).

**Triage:**
1. `docker ps` — is `tv-questdb` still starting? Cold ILP-server start
   can be 20–40s on first run.
2. `make doctor` runs the 7-section health check; if section 4 (QuestDB)
   is RED, follow that runbook.
3. Wait until +60s; if not green by then, BOOT-02 fires and the app halts.

**Source:** `crates/app/src/infra.rs::wait_for_questdb_ready`

## BOOT-02 — boot deadline exceeded — HALTING

**Trigger:** Item 7's `wait_for_questdb_ready` exceeded 60 seconds.
Severity::Critical. The app HALTS rather than start a tick-processing
pipeline that cannot persist. Operator action required.

**Triage:**
1. Confirm QuestDB is reachable: `nc -z 127.0.0.1 9009`.
2. Inspect `data/logs/auto-up.YYYY-MM-DD-HHMM.log` for the
   `docker compose up -d` background log.
3. After QuestDB is up, restart the app — boot will succeed within 10s.

**Source:** `crates/app/src/infra.rs::wait_for_questdb_ready`

## AUDIT-01 — Phase 2 audit row write failed

**Trigger:** `Phase2AuditWriter::append` ILP write failed. Audit tables
are SEBI-relevant — write failures must surface immediately.

**Triage:** check QuestDB ILP TCP port + disk-full state.
**Source:** `crates/storage/src/phase2_audit_persistence.rs`

## AUDIT-02 — depth-rebalance audit row write failed

**Trigger:** `DepthRebalanceAuditWriter::append` failed.
**Source:** `crates/storage/src/depth_rebalance_audit_persistence.rs`

## AUDIT-03 — WS reconnect audit row write failed

**Trigger:** `WsReconnectAuditWriter::append` failed.
**Source:** `crates/storage/src/ws_reconnect_audit_persistence.rs`

## AUDIT-04 — boot audit row write failed

**Trigger:** `BootAuditWriter::append` failed.
**Source:** `crates/storage/src/boot_audit_persistence.rs`

## AUDIT-05 — selftest audit row write failed

**Trigger:** `SelftestAuditWriter::append` failed.
**Source:** `crates/storage/src/selftest_audit_persistence.rs`

## AUDIT-06 — order audit row write failed

**Trigger:** `OrderAuditWriter::append` failed. SEBI 5-year retention
applies to this table.
**Source:** `crates/storage/src/order_audit_persistence.rs`

## STORAGE-GAP-03 — audit-table write failure (any table)

**Trigger:** any audit-table writer hit an unrecoverable error after the
ring + spill backoff exhausted. Coalesces AUDIT-01..06.

**Triage:** see specific AUDIT-NN code emitted alongside.
**Source:** `crates/storage/src/{phase2,depth_rebalance,ws_reconnect,boot,selftest,order}_audit_persistence.rs`

## STORAGE-GAP-04 — S3 archive failure

**Trigger:** the partition manager attempted to archive a detached
QuestDB partition to S3 and the upload failed. Idempotency-key
`(table, partition_date)` in `s3_archive_log` ensures retry-safety.

**Triage:**
1. AWS credentials valid? `aws sts get-caller-identity`.
2. S3 bucket reachable? `aws s3 ls s3://<bucket>/`.
3. Check Glacier 90-day minimum was honored — partition not re-archived.

**Source:** `crates/storage/src/s3_archive.rs` (Wave 2 Item 9.4)

## DISK-WATCHER-01 — spill disk-health watcher respawned (zero-tick-loss PR-5, G3)

**Trigger:** the spill disk-health watcher task
(`spawn_spill_disk_health_watcher`) exited — panic or external cancel — and
the supervisor (`spawn_supervised_spill_disk_health_watcher`) caught the
death, logged it, incremented `tv_disk_watcher_respawn_total{reason}`, and
respawned the watcher after a short backoff so disk-free monitoring
continues. The watcher is the early-warning for the single highest-risk
gap in the zero-loss chain ("disk full **and** QuestDB down at once"), so
losing it silently — the pre-PR-5 behaviour, where the handle was bound to
`_` in `main.rs` — meant the operator could lose `tv_spill_dir_free_bytes`
visibility with no signal. Severity::Low (the respawn self-heals); the
CloudWatch alarm `tv-<env>-disk-watcher-respawn` pages only on a *flapping*
watcher (Sum > 0 over 5m), not on a benign one-off.

**Why mirror WS-GAP-05 and not just restart the app:** the watcher is
stateless and cheap to respawn, and monitoring MUST keep running — a single
panic should not take disk visibility offline until the next manual restart.
This is the same supervisor pattern as the WS-GAP-05 pool supervisor.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — search for `DISK-WATCHER-01`; the
   event carries `reason` (`panic` / `cancelled` / `clean_exit` / `unknown`)
   and the spill `path`.
2. `reason="panic"` repeating → a real bug in `probe_disk_free_bytes` or the
   watcher loop (it has no `unwrap`/`expect`, so investigate the `df`
   shell-out path + parse). Inspect `data/logs/errors.jsonl.*` for the panic
   backtrace immediately preceding the DISK-WATCHER-01 line.
3. `reason="cancelled"` at shutdown → benign (the runtime is tearing down).
4. A sustained non-zero `tv_disk_watcher_respawn_total` rate (the CloudWatch
   alarm fired) with no obvious bug → restart the app to reset the watcher
   from a clean state.

**Auto-triage safe:** YES (Severity::Low; respawn already restored
monitoring — the operator inspects the `reason` + backtrace at leisure).

**Source:** `crates/storage/src/disk_health_watcher.rs::spawn_supervised_spill_disk_health_watcher`,
`crates/common/src/error_code.rs::DiskWatcher01Respawned`. Boot wiring:
`crates/app/src/main.rs` (the `_disk_health_watcher_supervisor` spawn).
