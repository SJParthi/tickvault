---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/core/src/websocket/connection.rs"
  - "crates/core/src/websocket/connection_pool.rs"
  - "crates/core/src/pipeline/tick_gap_detector.rs"
  - "crates/core/src/websocket/rate_limit_cooldown.rs"
  - "crates/app/src/main.rs"
  - "crates/app/tests/ws_rate_limit_cooldown_wiring_guard.rs"
  - "crates/core/src/websocket/types.rs"
  - "crates/core/src/websocket/order_update_connection.rs"
  - "crates/core/src/auth/token_manager.rs"
  - "crates/app/src/infra.rs"
  - "crates/storage/src/phase2_audit_persistence.rs"
  - "crates/storage/src/depth_rebalance_audit_persistence.rs"
  - "crates/storage/src/ws_reconnect_audit_persistence.rs"
  - "crates/storage/src/boot_audit_persistence.rs"
  - "crates/storage/src/selftest_audit_persistence.rs"
  - "crates/storage/src/order_audit_persistence.rs"
  - "crates/app/src/order_observability.rs"
  - "crates/storage/src/pnl_audit_persistence.rs"
  - "crates/storage/src/s3_archive.rs"
  - "crates/storage/src/disk_health_watcher.rs"
---

# Wave 2 Error Codes

> **Authority:** This file is the runbook target for the new ErrorCode
> variants added in the Wave 2 hardening implementation. The cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## WS-GAP-04 — main-feed WebSocket entered post-close sleep

> **C4 sweep note (2026-07-15):** the `WsGap04PostCloseSleep` variant SURVIVES
> — it is still emitted by the retained-dormant `order_update_connection.rs`
> (scope-lock §A.1); only the main-feed emit sites described below died with
> the lane. Content retained for historical audit.

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

> **[ARCHIVED 2026-07-20]** WS-GAP-05, WS-GAP-06, WS-GAP-07, WS-GAP-08, WS-GAP-09 (variants RETIRED in the C4 sweep 2026-07-15 — Dhan live-WS lane deleted; crossref-allowlisted; retained for historical audit) — moved verbatim to `docs/rules-archive/wave-2-error-codes-archive.md` (context-size incident; content unchanged).
## WS-GAP-10 — order-update WebSocket in-market outage (in-loop HIGH page)

> **⚠ 2026-07-14 UPDATE (operator Dhan noise lock —
> `dhan-rest-only-noise-lock-2026-07-14.md` +
> `websocket-connection-scope-lock.md` §A.1):** the `dhan_rest_stack`
> Phase 5a order-update spawn is CUT — NO live caller of
> `run_order_update_connection` remains on a dhan-off boot (the legacy
> fast-arm/lane sites are Dhan-gated dead code deleted by the Phase C-2
> PR). The core module `order_update_connection.rs` is RETAINED DORMANT
> (unit tests stay) for the live-trading re-wire, so this section's
> WS-GAP-10 machinery is dormant code, not a live pager. The two
> CloudWatch alarms (`tv-<env>-order-update-ws-inactive`,
> `tv-<env>-order-update-reconnect-storm`) are DELETED with the spawn.
> Content below retained as historical audit + the re-wire reference.

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
backstop. Clean-close flaps (server Close frame / stream end — Dhan's OTHER
documented auth-rejection delivery mode) COUNT toward the same 3-failure
page since the 2026-07-06 hostile-review fix: any session that ends BEFORE
the 60s stability window increments the streak regardless of delivery mode
(`streak_after_clean_close`), the page decision is evaluated in BOTH
reconnect-loop arms via the shared `emit_in_market_outage_page` helper, and
a mixed regime (≤2 errors then one clean close, repeating) can no longer
perpetually defeat the threshold. Only a stability-window survival resets
the streak, so normal idle-day clean closes (sessions living minutes/hours)
never count. Off-hours clean-close flaps enter the same WS-GAP-04
sleep-until-open after the attempt budget as the error regime. A
pathological connect → survive-60s → die metronome re-pages at most ~1 HIGH
per ~61s worst case (stability window + 3-failure ladder) — bounded, and
each cycle is a genuine outage + recovery. An
outage shorter than 3 failures pages nothing (absorbed by the 0/0/500ms
backoff ladder).

**Auto-triage safe:** YES (the loop self-retries; triage inspects — the page
is the visibility, not an action request).

**Source:** `crates/core/src/websocket/order_update_connection.rs::{run_order_update_connection, should_page_outage, streak_after_clean_close, should_emit_stable_recovery, emit_in_market_outage_page}`
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

### 2026-07-14 rebuild (cluster-C order-side observability)

The `order_audit` module was REBUILT on the modern
`rest_fetch_audit_persistence.rs` template (the pre-deletion source is
history only):

- **DEDUP key (verbatim):** `ts, trading_date_ist, order_id, leg, event, feed`
  — `order_id` is the identity (NOT `security_id`; the I-P1-11 pair does
  not apply to order events — one order has one instrument, and two
  lifecycle events for the same order at the same instant must BOTH
  survive, hence `event` in-key). `feed` per the feed-in-key override.
- **Transport:** ILP-over-HTTP with per-flush server ACK (the 2026-07-05
  fire-and-forget lesson); failed flush → `discard_pending`
  (poisoned-buffer defense, `tv_order_audit_rows_discarded_total`).
- **Stages on the `error!` (`code = AUDIT-06`):** `ensure_client_build` /
  `ensure_ddl` (HTTP-CLIENT-01-class duplicate-row window until a later
  ensure succeeds) / `append` / `flush` / `sink_drop` (the bounded
  order-side channel refused a message — the row AND its Telegram page
  are lost for that one event; best-effort forensics, the order path is
  unaffected).
- **Delivery boundary (honest):** AUDIT-06 is **log-sink-only** — no
  `error_code_alerts` map entry; the operator signals are the order-side
  sink's typed Telegram events + the daily OMS-GAP-02 reconcile verdict.
  Adding a CW log filter is a flagged follow-up.
- **Counters:** `tv_order_audit_rows_total{event}`,
  `tv_order_audit_persist_errors_total{stage}`,
  `tv_order_audit_nonfinite_clamped_total`,
  `tv_order_alert_dropped_total{reason}`.
- Consumer: `crates/app/src/order_observability.rs` (paper rows TODAY —
  `mode = "paper"` while `dry_run = true`).
- **Rejection-class split (C4, 2026-07-14 hostile review):** place-time
  API rejections (the `place_order` Err arm — OrderRejected Telegram +
  `rejected` audit row + since the C4 fix `tv_orders_rejected_total`) and
  WS-reported REJECTED transitions (`process_order_update` — counter/alarm
  only, NO Telegram/row; a `fire_alert` at that transition is a Phase-1
  follow-up) are DISJOINT classes — the orders-rejected alarm and the
  OrderRejected Telegram are two routes, not one signal chain.

## STORAGE-GAP-03 — audit-table write failure (any table)

**Trigger:** any audit-table writer hit an unrecoverable error after the
ring + spill backoff exhausted. Coalesces AUDIT-01..06.

**Triage:** see specific AUDIT-NN code emitted alongside.
**Source:** `crates/storage/src/{phase2,depth_rebalance,ws_reconnect,boot,selftest,order}_audit_persistence.rs`

### 2026-07-14 note — pnl_audit rebuild emits this code (cluster-C)

`crates/storage/src/pnl_audit_persistence.rs` (rebuilt on the same
ILP-over-HTTP template as order_audit above) emits STORAGE-GAP-03 with
stages `ensure_client_build` / `ensure_ddl` / `append` / `flush`. DEDUP
key: `ts, trading_date_ist, security_id, exchange_segment, snapshot_kind,
feed` (I-P1-11 pair + `snapshot_kind` + `feed` in-key). **OnEod heartbeat
contract (honest envelope — C1 truth-sync, 2026-07-14):** WHILE the
order-side consumer runs, it writes ≥1 `snapshot_kind = "on_eod"` row per
trading day (aggregate sentinels `security_id = 0`, `exchange_segment =
"ALL"`) at the market-close message — the positive "the pnl audit leg is
alive" signal (audit Rule 11) and the daily reconcile denominator. The
consumer + close signal are spawned by the trading pipeline, whose BOTH
spawn sites are Dhan-lane-gated — the subsystem is code-ready but DORMANT
while `feeds.dhan_enabled = false` (today's prod default): on a dhan-off
boot no row lands and nothing false-pages (nothing runs); the contract
activates whenever the Dhan lane / live trading runs. `on_fill` /
`on_minute` are Phase-1 emits (documented, dormant). Log-sink-only
delivery boundary, same as AUDIT-06.

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
