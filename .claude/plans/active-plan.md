# Implementation Plan: Session 8 — Real-Proof Resilience + Dhan Re-Verification

**Status:** DRAFT
**Date:** 2026-04-14
**Branch:** `claude/fix-gaps-verify-all-SsrE2`
**Approved by:** pending
**Previous session archived:** `archive/2026-04-14-session-7-aws-and-tv-doctor.md`

## Goal

Turn every "works on paper" resilience guarantee into a **real, executed, test-proven** guarantee. Specifically:

1. Prove zero-tick-loss under real QuestDB outage (not a mock).
2. Prove the WebSocket pool can't silently stall or reconnect-loop.
3. Prove every critical failure path emits a Telegram alert, with no silent degradation.
4. Re-verify the Dhan v2 docs + Python SDK against our `docs/dhan-ref/*.md` and `.claude/rules/dhan/*.md`, primary = Dhan docs, secondary = DhanHQ-py.
5. Wire the still-dormant option-chain / greeks pipeline against the re-verified mapping.
6. Lock in the scoped-testing default so no future session wastes 15 min on unrelated crates.

**Out of scope (by design):** adding Playwright, refactoring unrelated code, anything touching live trading (still sandbox-only until 2026-06-30).

## Open questions (need answer before starting)

1. **"Stop the rolling option alone"** — clarify. The codebase has no "rolling options" feature. Do you mean:
   (a) disable an auto-roll on Thursday for index options,
   (b) disable a rolling-window indicator,
   (c) remove a strategy flag from `config/base.toml`, or
   (d) something else?
   → **Blocks Block F only. All other blocks can proceed without an answer.**

2. **Pre-push coverage gate** — `cargo llvm-cov` takes ~90s even scoped. OK to add it as a pre-push step even though it slows pushes? If not, I'll keep coverage CI-only.

## Plan items

Each item commits on its own. Push at the end. Every item lists Files (what gets touched) and Tests (what has to exist and pass).

### BLOCK A — Prove zero tick loss with real chaos (not mocks)

- [ ] A1: Make `chaos_docker_questdb_kill_and_restart` a real test (currently a skeleton).
  - Files: `crates/storage/tests/chaos_questdb_lifecycle.rs`, `crates/storage/src/tick_persistence.rs`
  - Behaviour: spawn a short-lived `tv-questdb` container via `testcontainers-rs`, ingest 5,000 synthetic ticks, `docker kill` the container mid-stream, keep feeding ticks for 20s (they land in ring buffer → disk spill), start container again, assert every single pre- and mid-kill tick is present in QuestDB after recovery. Runs under `CI_WITH_DOCKER=1` locally and in CI.
  - Tests: `test_docker_questdb_kill_mid_stream_zero_loss`, `test_docker_questdb_never_recovers_all_ticks_on_disk`

- [ ] A2: Disk-full chaos — DLQ fallback proof.
  - Files: `crates/storage/tests/chaos_disk_full.rs` (new), `crates/storage/src/tick_persistence.rs`
  - Behaviour: mount a tmpfs of size `TICK_SPILL_MIN_DISK_SPACE_BYTES + 4 KiB`, fill it to 100%, ingest ticks, assert DLQ records are written (not silently dropped), free space, assert DLQ drain produces every tick in QuestDB. Unix-only, `#[cfg_attr(not(target_os = "linux"), ignore)]`.
  - Tests: `test_spill_writer_disk_full_goes_to_dlq`, `test_dlq_drain_produces_every_tick`

- [ ] A3: SIGKILL mid-batch replay.
  - Files: `crates/storage/tests/chaos_sigkill_replay.rs` (new)
  - Behaviour: fork a child process that runs `tick-ingest-smoke`, stream 10,000 ticks, send SIGKILL at tick 5,000, restart, assert spill file is replayed on startup and every pre-kill tick is in QuestDB. Unix-only.
  - Tests: `test_sigkill_mid_batch_replay_zero_loss`

- [ ] A4: Telegram alert on disk spillover (closes the "silent failure" gap from the audit).
  - Files: `crates/storage/src/tick_persistence.rs`, `crates/core/src/notification/service.rs`
  - Behaviour: when the ring buffer crosses `TICK_BUFFER_SPILLOVER_TRIGGER_PCT` (80%), emit `NotificationEvent::DiskSpillActive { depth, capacity }` at WARN. When the disk spill file grows past `TICK_SPILL_WARN_BYTES` (50 MiB), emit CRITICAL. Telegram service already wired; this adds the event + call site.
  - Tests: `test_spillover_emits_warn_notification`, `test_large_spill_emits_critical_notification`

- [ ] A5: Live intraday backfill worker (backlog item A3, now in scope).
  - Files: `crates/core/src/pipeline/backfill_worker.rs` (new), `crates/trading/src/risk/tick_gap_tracker.rs`, `crates/storage/src/tick_persistence.rs` (`append_tick` accepts `backfilled: bool`), `crates/app/src/trading_pipeline.rs`
  - Behaviour: when `TickGapTracker` emits ERROR-level gap, publish `GapDetected { security_id, from_ltt, to_now }` on bounded mpsc(1024). `backfill_worker` consumes, calls historical intraday REST (existing client), inserts resulting ticks with `backfilled=true`. Idempotent via existing QuestDB DEDUP.
  - Tests: `test_backfill_triggered_on_error_gap`, `test_backfill_idempotent_on_replay`, `test_backfill_respects_rate_limit`, `test_backfill_warmup_suppressed`

### BLOCK B — Prove WebSocket pool can't silently stall

- [ ] B1: Reconnect jitter (prevent thundering-herd on all 5 connections).
  - Files: `crates/core/src/websocket/connection.rs`, `crates/common/src/constants.rs`
  - Behaviour: existing 30s throttle gets `±5s` uniform jitter. Adds `rand` to the crate (already in workspace `Cargo.toml`).
  - Tests: `test_reconnect_delay_has_jitter`, `test_reconnect_delay_within_bounds`

- [ ] B2: Stall guard separate from gap detection.
  - Files: `crates/core/src/websocket/connection.rs`, `crates/core/src/pipeline/tick_processor.rs`
  - Behaviour: a per-connection "last-bytes-received" watchdog. If no bytes arrive for `WS_STALL_TIMEOUT_SECS` (45s during market hours, 900s outside), force reconnect AND emit CRITICAL. This is orthogonal to tick-sequence gap detection; it covers "connection is up but server stopped writing".
  - Tests: `test_stall_guard_triggers_reconnect`, `test_stall_guard_emits_critical`, `test_stall_guard_suppressed_outside_market_hours`

- [ ] B3: Pool-level degradation alert (backlog A4).
  - Files: `crates/core/src/websocket/connection_pool.rs`, `crates/core/src/websocket/pool_watchdog.rs`
  - Behaviour: when ALL `num_connections` are `Reconnecting` simultaneously for >60s → CRITICAL Telegram + `app_health = Degraded`. >300s → `exit(1)` for systemd restart.
  - Tests: `test_pool_degraded_when_all_reconnecting`, `test_pool_halt_after_300s_all_down`, `test_pool_recovers_cancels_halt_timer`

- [ ] B4: Graceful unsubscribe on SIGTERM (backlog A5).
  - Files: `crates/core/src/websocket/connection.rs`, `crates/core/src/websocket/connection_pool.rs`, `crates/app/src/main.rs` (shutdown hook)
  - Behaviour: on SIGTERM, send `FeedRequestCode::Disconnect (12)` to every WS before closing, with a 2s per-connection timeout. Best effort — timeout cannot block shutdown.
  - Tests: `test_graceful_unsubscribe_on_shutdown`, `test_unsub_timeout_does_not_block_shutdown`

### BLOCK C — Close the last "silent degradation" paths

- [ ] C1: SSM bootstrap fail-fast.
  - Files: `crates/core/src/notification/service.rs`, `crates/core/src/auth/secret_manager.rs`
  - Behaviour: if the secret manager cannot fetch the Telegram bot token or Dhan credentials at boot, panic with a clear message. Today it falls back to no-op — that's the "you didn't notify me because the notifier itself crashed" class of bug you called out. Panic = systemd restart = alertable.
  - Tests: `test_ssm_missing_token_panics`, `test_ssm_recoverable_error_retries_then_panics`

- [ ] C2: AlertManager → Telegram wiring verification + runbook.
  - Files: `deploy/docker/alertmanager/alertmanager.yml`, `docs/runbooks/alertmanager-telegram.md` (new)
  - Behaviour: verify the existing chain Prometheus → AlertManager → Telegram actually fires. Add a self-test alert rule that fires every `make alert-smoke-test` and asserts Telegram received it. Document the flow end-to-end.
  - Tests: `test_alertmanager_config_has_telegram_receiver`, `test_alertmanager_routes_critical_to_telegram`

- [ ] C3: Grafana alert rules for DLQ / spillover / pool degraded (backlog E2).
  - Files: `deploy/docker/grafana/provisioning/alerting/tickvault.yaml`
  - Rules: DLQ > 0 → CRITICAL; spill depth > 50% capacity → WARN; pool degraded > 60s → CRITICAL; sandbox gate block → INFO.
  - Tests: `test_grafana_alerts_yaml_valid`, `test_grafana_alerts_reference_existing_metrics`

- [ ] C4: New metrics wired to Prometheus (backlog E1).
  - Files: `crates/core/src/pipeline/backfill_worker.rs`, `crates/storage/src/tick_persistence.rs`
  - Counters/gauges: `tv_backfill_ticks_total`, `tv_backfill_errors_total`, `tv_dlq_ticks_total`, `tv_pool_degraded_seconds_total`, `tv_sandbox_gate_blocks_total`.
  - Tests: `test_new_metrics_registered`, `test_backfill_metric_increments`

### BLOCK D — Dhan v2 + Python SDK re-verification (primary = Dhan docs)

- [ ] D1: Fetch every URL in the user's list with WebFetch, parse, write a per-section diff vs `docs/dhan-ref/` and `.claude/rules/dhan/`.
  - Files: `docs/dhan-ref/verification-2026-04-14.md` (new)
  - Behaviour: WebFetch each URL, extract the sections we reference, compute a structural diff (field names, types, enum values, byte offsets, packet sizes, rate limits, endpoint URLs). Report PASS / DIFF per section. **No rule file edits in this pass** — deltas become follow-up items in the same session.
  - Tests: `test_verification_report_exists`, `test_verification_report_covers_every_rule_file`

- [ ] D2: Clone DhanHQ-py read-only, diff the binary parsing constants (packet sizes, byte offsets) against `crates/common/src/constants.rs` and the parser modules.
  - Files: `docs/dhan-ref/verification-2026-04-14.md` (appends Python SDK section)
  - Behaviour: `git clone --depth 1 https://github.com/dhan-oss/DhanHQ-py /tmp/dhanhq-py`, `grep` the `dhanhq/marketfeed` files for packet sizes, report diff. Read-only — no code copied.
  - Tests: (same as D1 — one report file)

- [ ] D3: Apply **CRITICAL** deltas found in D1/D2 to rules + code (if any).
  - Files: TBD based on the report. Worst case: a few byte offsets in parsers.
  - Tests: whatever parser tests the deltas touch.
  - **Note:** this item's size depends entirely on what D1/D2 find. If they find nothing, this item is a no-op and gets checked off immediately.

- [ ] D4: Option-chain + Greeks live pipeline wiring (closes the "unimplemented" gap from the audit).
  - Files: `crates/core/src/option_chain/client.rs` (new), `crates/core/src/option_chain/parser.rs` (new), `crates/trading/src/indicator/greeks.rs`, `crates/api/src/handlers/option_chain.rs`, `crates/app/src/trading_pipeline.rs`
  - Behaviour: implement option chain REST client per `.claude/rules/dhan/option-chain.md` (PascalCase fields, decimal strike keys, `client-id` header, `Option<CE/PE>`). Parse response into the existing `OptionChainData` type. Feed into Greeks engine at a 3s cadence (rate-limit from the rule). Expose via `/api/option-chain` and `/api/pcr`.
  - Tests: `test_option_chain_request_headers`, `test_option_chain_parses_decimal_strike_keys`, `test_option_chain_handles_none_ce_pe`, `test_option_chain_rate_limit_3s`, `test_greeks_computed_on_chain_update`, `test_pcr_endpoint_returns_valid_response`

- [ ] D5: Full Market Depth (20/200-level) subscription validator (defensive, closes the "code swap" gap).
  - Files: `crates/core/src/websocket/subscription_builder.rs`, `crates/core/src/parser/deep_depth.rs`
  - Behaviour: assert at subscription build time that a 200-level subscription has exactly 1 instrument; reject with a typed error otherwise. Assert parsed response codes are `41` (bid) or `51` (ask) before routing to the depth buffer; log + skip anything else.
  - Tests: `test_200level_subscription_rejects_multi_instrument`, `test_depth_parser_rejects_unknown_response_code`

### BLOCK E — Scoped-testing lockdown + flaky fix

- [ ] E1: Ratchet scoped-testing into every rule file (clarify "scoped by default" at the top of each one so future sessions cannot assume workspace-wide).
  - Files: `.claude/rules/project/testing.md` (add a header banner), `.claude/rules/project/enforcement.md` (cross-reference), `CLAUDE.md` (already references — verify).
  - Tests: `test_testing_rule_banner_references_scope`, `test_enforcement_rule_references_scope`

- [ ] E2: Fix flaky `test_prolonged_outage_ring_plus_spill_zero_loss` (backlog F1).
  - Files: `crates/storage/src/tick_persistence.rs` (constructor accepts optional spill dir), `crates/storage/tests/tick_resilience.rs`
  - Behaviour: make `TICK_SPILL_DIR` configurable per-writer; each test uses a unique temp dir so parallel tests can't collide. Default stays `"data/spill"` for prod.
  - Tests: existing test runs 3× in a row under `--test-threads=8` without flake.

- [ ] E3: Pre-push coverage gate (optional — depends on Open Question 2).
  - Files: `.claude/hooks/pre-push-gate.sh`, `.claude/hooks/scoped-coverage-runner.sh` (new)
  - Behaviour: after scoped tests pass, run `cargo llvm-cov -p tickvault-<crate>` for each touched crate and assert the threshold in `quality/crate-coverage-thresholds.toml`. If you don't want this (too slow), skip — CI still enforces it.
  - Tests: `test_scoped_coverage_runner_reads_thresholds`, `test_scoped_coverage_runner_fails_below_threshold`

### BLOCK F — Cleanup and clarifications

- [ ] F1: "Rolling option" disable (BLOCKED on Open Question 1).

- [ ] F2: Playwright decision memo (one paragraph in `docs/standards/testing-tooling.md`).
  - Behaviour: document that Playwright is not adopted because tickvault has no UI surface beyond a static read-only portal, and browser-based testing adds CI time without catching real defects. The portal is tested via HTTP integration tests instead.

- [ ] F3: Final `plan-verify.sh` run + push session 8.
  - Files: `active-plan.md` (this file, Status → VERIFIED)
  - Tests: N/A — plan verify step.

## Mechanical enforcement additions

These are the "never let this happen again" knobs the plan itself introduces:

1. **Scoped testing** is already present; E1 locks it in writing.
2. **Disk spillover is no longer silent** (A4).
3. **WS pool degradation is no longer silent** (B3).
4. **SSM bootstrap failure is no longer silent** (C1).
5. **200-level subscription misuse is caught at build time, not runtime** (D5).
6. **Dhan re-verification is a committed artifact** (`verification-2026-04-14.md`) so the next session can diff against it.
7. **Sandbox hard-guard gets an explicit test** so it can't regress.

## Honest constraints

- I cannot run `docker kill` in this sandbox environment. For A1/A3/A5 I will mark them `#[ignore]`-gated behind `CI_WITH_DOCKER=1` and run them locally on your Mac after you approve. The assertions will be real; the orchestration just needs your Mac's docker socket.
- `cargo llvm-cov` pre-push adds ~60–90s per push on cold cache. E3 is optional for that reason.
- D1/D2 use WebFetch; if a Dhan URL is behind Cloudflare I'll note it in the report and fall back to the Python SDK for that section.
- Sandbox-only until 2026-06-30 is already enforced; I am not touching that code except to add the regression test.
- "Never a single tick dropped" is provable end-to-end AFTER Block A runs green on your Mac with the docker socket. I will not claim the guarantee until you see the test output.

## Proof I will deliver at the end (nothing less counts)

- Output of `bash .claude/hooks/plan-verify.sh` showing every item `[x]` and every listed test existing in the codebase.
- Output of `bash .claude/hooks/scoped-test-runner.sh` for every touched crate, all green.
- Output of `CI_WITH_DOCKER=1 cargo test -p tickvault-storage --test chaos_questdb_lifecycle`, all green (you run this on your Mac; I will provide the exact command and expected output).
- `verification-2026-04-14.md` with per-section PASS / DIFF.
- A summary diff of the 5 new Telegram alerts so you can see exactly what would fire and when.

## Approval

Reply **"go ahead"** (or with edits) and I will:
1. Answer Open Question 1 and 2 inline if you add them to the reply.
2. Move Status → APPROVED.
3. Execute block by block, committing after each item, pushing at the end.
