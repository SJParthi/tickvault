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
