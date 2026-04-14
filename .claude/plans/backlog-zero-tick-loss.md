# Zero Tick Loss Hardening — Backlog (Follow-up Sessions)

> **Status:** BACKLOG (not an active plan, not enforced by plan-verify.sh)
> **Parent branch:** `claude/websocket-zero-tick-loss-nUAqy`
> **Session 1 delivered:** A1 (verified), A2 (DLQ), B1 (chaos skeleton), D1/D2 (scoped testing). See `archive/2026-04-13-zero-tick-loss-session-1.md`.

Each item below becomes its own active plan in a future session.

## Hardening items

### A3 — Live intraday backfill worker
When `TickGapTracker` emits an ERROR-level gap (`gap_secs >= TICK_GAP_ERROR_THRESHOLD_SECS`), publish a `GapDetected { security_id, from_ltt, to_now }` event on a bounded mpsc. A new `backfill_worker` task consumes these, calls `historical_intraday_fetcher` for the gap window, inserts resulting synthetic ticks into `append_tick` with a `backfilled=true` column. Idempotent via existing QuestDB DEDUP.
- Files: `crates/trading/src/risk/tick_gap_tracker.rs`, `crates/core/src/pipeline/backfill_worker.rs` (new), `crates/storage/src/tick_persistence.rs`, `crates/app/src/trading_pipeline.rs`
- Tests: `test_backfill_triggered_on_error_gap`, `test_backfill_idempotent_on_replay`, `test_backfill_respects_rate_limit`, `test_backfill_warmup_suppressed`

### A4 — Pool-level circuit breaker
When ALL `num_connections` (default 5) WS connections are `Reconnecting` simultaneously for > 60s, emit CRITICAL Telegram + set `app_health = Degraded`. If > 300s, `exit(1)` for supervisor restart.
- Files: `crates/core/src/websocket/connection_pool.rs`
- Tests: `test_pool_degraded_when_all_reconnecting`, `test_pool_halt_after_300s_all_down`

### A5 — Graceful unsubscribe on shutdown
On SIGTERM, send `FeedRequestCode::Disconnect` (12) to every WS connection before closing, best-effort with 2s timeout each.
- Files: `crates/core/src/websocket/connection.rs`, `crates/core/src/websocket/connection_pool.rs`
- Tests: `test_graceful_unsubscribe_on_shutdown`, `test_unsub_timeout_does_not_block`

## Chaos / real-situation tests

### B2 — Disk-full chaos
New test `crates/storage/tests/chaos_disk_full.rs`. Fills spill dir to `TICK_SPILL_MIN_DISK_SPACE_BYTES`, asserts DLQ records are written by A2 path, frees space, asserts recovery drain. `#[ignore]`, unix-only.

### B3 — App SIGKILL mid-batch
New test `crates/storage/tests/chaos_sigkill_replay.rs`. Forks child process, streams ticks, SIGKILLs mid-stream, restarts child, asserts spill file is replayed on startup and every pre-kill tick is in QuestDB. `#[ignore]`, unix-only.

### B4 — Docker chaos harness
Expand `chaos_questdb_lifecycle::chaos_docker_questdb_kill_and_restart` (skeleton in place) to actually kill/restart a real `dlt-questdb` container. Needs `CI_WITH_DOCKER=1`.

## Doc verification

### C1 — Dhan doc + Python SDK re-verification
Fetch every URL from the user's list (WebFetch) + clone DhanHQ-py, diff against `docs/dhan-ref/*.md` and `.claude/rules/dhan/*.md`. Produce `docs/dhan-ref/verification-2026-04-13.md` with PASS/DIFF per file. Primary = Dhan docs, secondary = Python SDK. NO rule file edits in this pass — diffs become separate follow-up items.

## Observability

### E1 — Prometheus metrics for new paths
Add counters/gauges: `dlt_backfill_ticks_total`, `dlt_backfill_errors_total`, `dlt_dlq_ticks_total` (already emitted from A2 — just wire into scrape), `dlt_pool_degraded_seconds_total`, `dlt_sandbox_gate_blocks_total`.

### E2 — Grafana alert rules
Append to `deploy/docker/grafana/provisioning/alerting/*.yaml`:
- DLQ > 0 → CRITICAL (every dead-letter is a data integrity incident)
- Spill depth > 50% capacity → WARN
- Pool degraded > 60s → CRITICAL
- Sandbox gate block → INFO

### E3 — Grafana dashboards for new metrics
Panels for: ring buffer depth over time, spill growth rate, reconnect count, DLQ lines over time, gap events per security.

## Pre-existing flake (surfaced during session 1)

### F1 — Fix tick_resilience parallel race
`crates/storage/tests/tick_resilience.rs::test_prolonged_outage_ring_plus_spill_zero_loss` is flaky under parallel execution. Multiple tests in the file share `data/spill/ticks-YYYYMMDD.bin`. Fix options:
1. Make `TICK_SPILL_DIR` configurable per-writer (best — constructor takes an optional override path). Default stays `"data/spill"`.
2. Mark race-sensitive tests with `#[serial]` via the `serial_test` crate (pin version in workspace Cargo.toml).

Option 1 is cleaner and also improves real-world deployment (AWS will use a dedicated spill volume).

## Incidentals queued for future

- 118 pre-existing clippy warnings in `crates/storage/` — these run in CI only but some are legitimate (e.g., `manual_range_contains`). Batch-fix in a dedicated cleanup session.
- Depth persistence writer (`DepthPersistenceWriter`) does NOT have a DLQ fallback. A2 was scoped to ticks only. Mirror the pattern for depth in a follow-up.
