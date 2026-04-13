# Implementation Plan: Zero Tick Loss Session 2 — Full Backlog

**Status:** APPROVED
**Date:** 2026-04-13
**Approved by:** Parthiban ("everything bro")
**Branch:** `claude/websocket-zero-tick-loss-nUAqy`
**Session 1 archived:** `archive/2026-04-13-zero-tick-loss-session-1.md` (VERIFIED, 4 commits pushed)
**Parent backlog:** `backlog-zero-tick-loss.md`

## Goal

Burn down the entire zero-tick-loss backlog: F1 + A3 + A4 + A5 + B2 + B3 + C1 + E1 + E2. Commit after each item so any mid-session interruption leaves a clean tree.

## Order of execution (dependency-aware)

1. **F1** — parallel-test flake fix (quick win, independent, validates scope-config pattern)
2. **A5** — graceful unsubscribe on shutdown (small, surgical)
3. **A4** — pool-level circuit breaker (self-contained)
4. **A3** — backfill worker on tick-gap events (biggest, most dependencies)
5. **E1** — Prometheus metrics wiring for A2/A3/A4 outputs
6. **E2** — Grafana alert rules referencing E1 metrics
7. **B2** — disk-full chaos test (`#[ignore]`)
8. **B3** — SIGKILL chaos test (`#[ignore]`)
9. **C1** — Dhan doc + Python SDK verification diff (read-only, no code changes to rule files)

## Extreme automation requirements (applies to every item)

Every item must satisfy all 8 rows from session 1:
auto-track, auto-monitor, auto-debug, auto-log, auto-capture, auto-resolve, auto-alert, auto-configurable.

## Plan items

- [x] F1. **Parallel-test flake fix — configurable TICK_SPILL_DIR.** Added `spill_dir: PathBuf` field to `TickPersistenceWriter`, default `TICK_SPILL_DIR`, public `set_spill_dir_for_test()` setter. Plumbed through `open_spill_file`, `open_dlq_file`, `recover_stale_spill_files`, and a new `available_disk_space_bytes_for(dir)` helper. Migrated `test_prolonged_outage_ring_plus_spill_zero_loss` to use a per-test tmp dir. Production default unchanged.
  - Files: crates/storage/src/tick_persistence.rs (spill_dir field, setter, 4 call sites), crates/storage/tests/tick_resilience.rs (per-test dir override)
  - Tests: test_custom_spill_dir_used_for_spill ✅, test_custom_spill_dir_used_for_dlq ✅, tick_resilience full suite 12/12 ✅

- [x] A5. **Graceful unsubscribe on shutdown.** Added `shutdown_requested: AtomicBool` + `shutdown_notify: Arc<Notify>` to `WebSocketConnection`. The `run_read_loop` now uses `tokio::select!` with a biased shutdown arm that sends `{"RequestCode":12}` as a Text frame, waits 2s max for ack, then closes the socket. Pool exposes `request_graceful_shutdown()` that signals all connections and logs live/dead split. Metrics: `dlt_ws_graceful_unsub_total{outcome=sent|send_failed|timeout}`, `dlt_ws_graceful_shutdown_signalled_total`.
  - Files: crates/core/src/websocket/connection.rs (fields, getter, read-loop select!), crates/core/src/websocket/connection_pool.rs (pool-level wrapper)
  - Tests: test_graceful_shutdown_sets_flag_and_notifies ✅, test_graceful_shutdown_sends_disconnect_request ✅, test_graceful_shutdown_timeout_does_not_block ✅, test_pool_graceful_shutdown_signals_all_connections ✅, test_pool_graceful_shutdown_skips_dead_connections_safely ✅

- [x] A4. **Pool-level circuit breaker.** New `pool_watchdog.rs` module implements a pure state machine (`Healthy` / `AllDown{since}`) with thresholds `POOL_DEGRADED_ALERT_SECS=60` and `POOL_HALT_SECS=300`. Returns `WatchdogVerdict::{Healthy, Recovered, Degrading, Degraded, Halt}`. `WebSocketConnectionPool::poll_watchdog()` wires the watchdog to metrics (`dlt_pool_degraded_seconds`, `dlt_pool_degraded_alerts_total`, `dlt_pool_halts_total`, `dlt_pool_recoveries_total`) and ERROR-level tracing (which fires Telegram via the existing ERROR hook). Caller is expected to poll every ~5s from a background task and exit on `WatchdogVerdict::Halt`.
  - Files: crates/core/src/websocket/pool_watchdog.rs (new), crates/core/src/websocket/mod.rs, crates/core/src/websocket/connection_pool.rs (watchdog field + poll_watchdog method)
  - Tests: 11 watchdog unit tests + 2 pool integration tests. Watchdog: starts_healthy ✅, all_connected_stays_healthy ✅, one_alive_is_healthy ✅, detects_all_reconnecting_as_degrading ✅, degrades_at_60s ✅, halts_at_300s ✅, recovery_resets_state ✅, fires_degraded_alert_exactly_once_per_cycle ✅, disconnected_counts_as_down ✅, connecting_counts_as_live ✅, empty_pool_is_degrading ✅. Pool: poll_watchdog_initial_state_is_degrading ✅, poll_watchdog_stable_across_polls ✅

- [ ] A3. **Live intraday backfill worker.** When `TickGapTracker` emits an ERROR-level gap, publish a `GapDetected { security_id, from_ltt, to_now }` event on a bounded mpsc. A new `backfill_worker` task consumes events, calls the existing `HistoricalCandleFetcher::fetch_intraday_minute` for the gap window, synthesises `ParsedTick` records from minute OHLCV bars (one tick per minute, LTP=close, LTT=candle timestamp), pushes them through the normal `append_tick` path with `backfilled=true` column. QuestDB DEDUP ensures idempotency on replay.
  - Files: tick_gap_tracker.rs, backfill_worker.rs (new), tick_persistence.rs (add backfilled column DDL + ILP field), trading_pipeline.rs
  - Tests: test_backfill_event_published_on_error_gap, test_backfill_worker_synthesises_ticks_from_candles, test_backfill_idempotent_via_dedup, test_backfill_rate_limit_respected, test_backfill_warmup_suppressed

- [ ] E1. **Prometheus metrics for new paths.** Counters/gauges emitted by A2/A3/A4. Most are already wired (dlt_dlq_ticks_total from session 1). Add: dlt_backfill_ticks_total, dlt_backfill_errors_total, dlt_pool_degraded_seconds_total, dlt_sandbox_gate_blocks_total (wire at config-validation point).
  - Files: backfill_worker.rs, connection_pool.rs, config.rs
  - Tests: covered via unit tests that assert metric names exist (recorder fixture)

- [ ] E2. **Grafana alert rules + dashboard panels.** New file `deploy/docker/grafana/provisioning/alerting/zero-tick-loss.yaml` with 5 alerts: DLQ>0 CRITICAL, spill>50% WARN, spill>90% CRITICAL, pool_degraded>60s CRITICAL, backfill_errors>10/min WARN. Dashboard JSON panels for each new metric.
  - Files: zero-tick-loss.yaml (new), trading-pipeline.json (append panels)
  - Tests: YAML syntax validation via serde_yaml roundtrip in a small helper test

- [ ] B2. **Disk-full chaos test.** `crates/storage/tests/chaos_disk_full.rs` — fills the spill dir to the low-disk threshold, appends ticks, asserts DLQ records appear and `dlq_ticks_total` increments. Frees space, asserts no further DLQ growth. `#[ignore]` by default, unix-only.
  - Files: chaos_disk_full.rs (new)
  - Tests: chaos_disk_full_triggers_dlq, chaos_disk_full_recovery

- [ ] B3. **SIGKILL mid-batch chaos test.** `crates/storage/tests/chaos_sigkill_replay.rs` — forks a child process that streams ticks to a fake QDB, SIGKILLs it mid-stream, restarts the child with the same spill_dir, verifies the spill file from the killed process is replayed on startup. `#[ignore]` + `CI_WITH_SIGKILL=1` gate.
  - Files: chaos_sigkill_replay.rs (new)
  - Tests: chaos_sigkill_spill_replay_zero_loss

- [ ] C1. **Dhan doc + Python SDK verification diff.** Read-only pass: fetch every URL in the session 1 reference list (WebFetch), diff against `docs/dhan-ref/*.md` and `.claude/rules/dhan/*.md`, produce `docs/dhan-ref/verification-2026-04-13.md` with PASS/DIFF per file. NO rule file edits this session — any diffs become follow-up items. Primary = Dhan docs, secondary = Python SDK.
  - Files: verification-2026-04-13.md (new, docs/dhan-ref/)
  - Tests: (read-only doc task, no code)

## Scenarios

| # | Scenario | Expected | Verified by |
|---|----------|----------|-------------|
| 1 | Parallel tick_resilience tests | No spill file collision | F1 + re-run existing suite |
| 2 | SIGTERM during active WS | Disconnect request (12) sent to every live connection before socket close | A5 |
| 3 | All 5 WS simultaneously reconnecting for 65s | Telegram CRITICAL + metric gauge updated | A4 |
| 4 | All 5 WS simultaneously reconnecting for 305s | Process exits with non-zero for supervisor restart | A4 |
| 5 | Single instrument 60s gap detected | GapDetected event → backfill_worker → historical fetch → synthetic ticks → DEDUP merges | A3 |
| 6 | Backfill same gap twice | Second backfill is a no-op (DEDUP) | A3 idempotency test |
| 7 | Disk full during outage | DLQ NDJSON records written, counter increments | B2 |
| 8 | SIGKILL during spill write | On restart, spill file drained to QDB | B3 |
| 9 | Dhan doc check | verification-2026-04-13.md produced with PASS/DIFF per ref file | C1 |

## Verification

```bash
bash .claude/hooks/plan-verify.sh
bash .claude/hooks/pub-fn-test-guard.sh .
bash .claude/hooks/scoped-test-runner.sh
cargo fmt --check
cargo test -p dhan-live-trader-storage -p dhan-live-trader-core -p dhan-live-trader-trading
```
