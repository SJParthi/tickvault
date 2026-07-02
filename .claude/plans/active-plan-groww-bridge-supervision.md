# Implementation Plan: supervise the Groww bridge + parse-time stall liveness

**Status:** VERIFIED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — standing directive this session ("deep research… cover all extreme permutations… fix"); these are the 2 remaining HIGHs from the 2026-07-02 four-agent adversarial sweep, queued after PR #1305/#1306 merged.

## Design

**HIGH-1 (bridge unsupervised):** `main.rs:504` spawns `run_groww_bridge` as a
bare `tokio::spawn`. A panic anywhere in the bridge (parse edge case, writer
bug) kills the ONLY consumer of the sidecar NDJSON silently: ticks keep
appending to disk but nothing persists, no candles seal, and the only signal is
the eventual stall watchdog — which then kills the WRONG process (the healthy
Python sidecar, repeatedly, forever). Fix: `spawn_supervised_groww_bridge`
(WS-GAP-05 / DISK-WATCHER-01 / FEED-SUPERVISOR-01 respawn pattern): an outer
task creates the Groww `MultiTfAggregator` ONCE and spawns the IST-midnight
force-seal ONCE (both hoisted out of `run_groww_bridge` so respawns never leak
duplicate force-seal tasks or lose in-memory bars), then loops: spawn the
bridge, await its `JoinHandle`, classify the exit (panic / clean return),
`error!(code = FEED-SUPERVISOR-01, component = "groww_bridge")`, increment
`tv_feed_supervisor_respawn_total{feed="groww", component="bridge"}`, back off
(5s doubling to 60s cap), respawn. A respawned bridge re-tails the NDJSON from
byte 0 — residual-neutral by the established deterministic-`capture_seq` DEDUP
collapse — and REUSES the same aggregator (in-memory bars survive the panic).
The existing `FeedSupervisor01Respawned` ErrorCode is reused (same respawn
semantics; its rule file gains the bridge source in this PR).

**HIGH-2 (persist-coupled stall liveness):** the sidecar stall watchdog
(`should_restart_on_stall`, supervisor:926) reads
`feed_health.last_tick_age_secs(Feed::Groww)`, which is updated ONLY by
`record_ticks` in the bridge's flush-`Ok` arm (`groww_bridge.rs:765`). A
QuestDB outage > `FEED_STALL_RESTART_SECS` therefore looks identical to a dead
socket: the watchdog kills the HEALTHY sidecar in a restart storm while the
real fault is the DB — and upstream NATS ticks are lost during the churn
windows. Fix: split liveness from persistence. New
`FeedHealthRegistry::record_feed_liveness(feed, ts)` updates ONLY the
last-tick timestamp; the bridge calls it at PARSE time (lines parsed from the
NDJSON this wake — proof the sidecar delivered), before and independent of the
flush. `record_ticks` stays in the flush arm so tick COUNTS remain
persist-honest. Result: DB down → liveness keeps advancing → no false
FEED-STALL-01 kill; sidecar actually dead → no parses → watchdog still fires
exactly as designed.

Scope check: Dhan's stall handling is NOT persist-coupled the same way (its
pool watchdog reads WS frame activity, not `record_ticks`), so this PR is
Groww-side only.

## Edge Cases
- Bridge panics mid-burst → respawn re-tails from 0; deterministic capture_seq
  → QuestDB DEDUP collapses the replay; aggregator (shared across respawns)
  keeps its bars; force-seal task unaffected (spawned once, same aggregator).
- Respawn storm (persistent panic) → 60s backoff ceiling; every respawn logs
  FEED-SUPERVISOR-01 + counter — a flapping bridge pages via the existing
  respawn-rate alarm semantics; never gives up.
- QuestDB down 5+ min during market hours → parse-time liveness advances (sidecar
  healthy) → NO sidecar kill; flush errors page separately (audit Rule 5).
- Sidecar dead + QuestDB down → no parses → liveness stalls → FEED-STALL-01
  still kills/relaunches the sidecar (correct — it IS the stalled component).
- Quiet market (no lines) → liveness does not advance — same as today (the
  watchdog's market-hours + known-last-tick gates already cover cold/quiet).
- Groww disabled → bridge idles as before; supervision adds no Groww work.

## Failure Modes
- Supervisor wrapper itself panics → it is the OUTER task; its death is the
  same class as today's bare spawn — mitigated by keeping the wrapper minimal
  (loop + match + sleep, no parsing logic).
- record_feed_liveness racing record_ticks → both are relaxed atomic stores of
  a timestamp; last-writer-wins is correct (both are "now").
- A future edit re-coupling liveness to flush → source-scan ratchet fails the
  build (parse-site call pinned before the flush arm).

## Test Plan
- feed_health.rs unit tests: `record_feed_liveness` updates last-tick age and
  does NOT bump tick counts; interleaving with record_ticks.
- groww_bridge.rs source-scan tests (mirroring its existing inline guards):
  (a) `spawn_supervised_groww_bridge` exists and main.rs calls it (not a bare
  `run_groww_bridge` spawn); (b) the parse-time `record_feed_liveness` call
  site appears BEFORE the flush arm; (c) the force-seal spawn lives in the
  supervisor wrapper (once), not inside the respawned bridge body.
- supervisor backoff: pure `bridge_respawn_backoff` unit tests (5s→60s cap).
- Full `cargo test -p tickvault-app --lib` + `-p tickvault-common` green.

## Rollback
`git revert` — no schema/config change; reverting restores the bare spawn +
persist-coupled liveness.

## Observability
- `tv_feed_supervisor_respawn_total{feed="groww", component="bridge"}` +
  `error!(code = FEED-SUPERVISOR-01)` per respawn (existing alarm semantics).
- Liveness split is visible on `/api/feeds/health`: last-tick age now reflects
  delivery (parse), tick counts reflect persistence — each honest.

## Plan Items
- [x] `record_feed_liveness` in FeedHealthRegistry + unit tests
  - Files: crates/common/src/feed_health.rs
  - Tests: test_record_feed_liveness_updates_age_not_counts
- [x] Parse-time liveness call in the bridge wake path
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_parse_time_liveness_is_recorded_before_flush
- [x] `spawn_supervised_groww_bridge` (aggregator + force-seal hoisted, respawn loop, backoff) + main.rs wiring
  - Files: crates/app/src/groww_bridge.rs, crates/app/src/main.rs
  - Tests: test_bridge_respawn_backoff_curve, test_spawn_supervised_groww_bridge_owns_aggregator_and_force_seal_once
- [x] Rule-file source update (FEED-SUPERVISOR-01 gains the bridge wrapper)
  - Files: .claude/rules/project/feed-stall-watchdog-error-codes.md
  - Tests: (doc)

## Post-implementation adversarial review (3-agent, 2026-07-02) — all findings fixed in-PR

- [x] H1: stale-token alert marker routed to AuthRejected (classify arm + test)
  - Files: crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_classify_access_token_stale_marker_is_auth_rejected
- [x] M2: cancel-aware supervisor exit + healthy-run backoff reset
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_next_respawn_count_resets_after_healthy_run
- [x] M3: process-lifetime GrowwAuditLatches + death-edge Disconnected emit
  - Files: crates/app/src/groww_bridge.rs, crates/app/tests/groww_live_pipeline_e2e.rs
  - Tests: test_runtime_disable_clears_connected_and_rearms_latches
- [x] M4: sidecar forces a fresh SSM token read every 5th non-auth failure
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: test_sidecar_reads_token_never_mints
- [x] M5: orphaned-sidecar reap before each launch (pkill by script path)
  - Files: crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_supervisor_injects_token_param_env_and_no_credentials
- Hot-path + security agents: CLEAN (0 findings); hostile L6/L7 accepted with documented reasoning.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Bridge panics at 11:00 IST | respawn ≤5s, NDJSON re-tail collapses via DEDUP, zero capture loss, FEED-SUPERVISOR-01 logged |
| 2 | QuestDB down 5 min, sidecar healthy | no sidecar kill; liveness advances at parse time; flush errors page |
| 3 | Sidecar socket silently dead | no parses → FEED-STALL-01 kill/relaunch fires exactly as before |
| 4 | Respawn storm | 60s ceiling, counter climbs, never gives up |
