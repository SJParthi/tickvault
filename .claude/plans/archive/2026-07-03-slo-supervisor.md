# Implementation Plan: SLO publisher supervisor — silent task death respawn (SLO-03)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — live incident 2026-07-03 10:35 IST (verified via CloudWatch: `tv_realtime_guarantee_score` last datapoint 10:35 IST mid-market, no `error!`, no respawn, app otherwise healthy; the guarantee-critical alarm false-OK'd via missing→NonBreaching and only `market-hours-liveness-missing` caught it)

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (per-wave-guarantee-matrix.md cross-reference form). Specific rows proved by
> this PR: 100% logging (`error!` with `code = ErrorCode::Slo03PublisherRespawned.code_str()`),
> 100% monitoring (`tv_slo_publisher_respawn_total{reason}` counter),
> 100% extreme check (source-scan wiring ratchet + ErrorCode ratchet suite),
> Real-time proof (the 10s `tv_realtime_guarantee_score` publisher can no
> longer vanish silently — task death now logs + counts + respawns).

## Design

The Wave 3-D SLO evaluator/publisher (the 10s task that emits
`tv_realtime_guarantee_score` + 6 per-dimension gauges; spawn site in
`crates/app/src/main.rs` inside `start_dhan_lane`, gated on
`config.features.realtime_guarantee_score`; pure evaluator in
`crates/core/src/instrument/slo_score.rs`) is today a bare `tokio::spawn`
whose `JoinHandle` is dropped. Any panic/cancel/clean-exit kills the metric
stream silently — exactly the 2026-07-03 10:35 IST incident.

Fix (mirrors `spawn_supervised_oom_monitor` / DISK-WATCHER-01 / WS-GAP-05):

1. Extract the inline SLO loop body into a module-level
   `fn spawn_slo_publisher_task(health, qdb_config, feed_runtime, main_feed_pool_size) -> JoinHandle<()>`
   in `crates/app/src/main.rs` (behaviour-identical move; no logic change to
   the loop itself).
2. Add `fn spawn_supervised_slo_publisher(...) -> JoinHandle<()>` — infinite
   supervisor loop: spawn inner task → `handle.await` → classify via
   `tickvault_storage::disk_health_watcher::classify_join_exit` →
   `error!(code = ErrorCode::Slo03PublisherRespawned.code_str(), reason, ...)`
   → `metrics::counter!("tv_slo_publisher_respawn_total", "reason" => reason)`
   → bounded backoff (`SLO_PUBLISHER_RESPAWN_BACKOFF_SECS = 5`) → respawn.
3. Process-global once-guard (`static SLO_SUPERVISOR_SPAWNED: AtomicBool`):
   `start_dhan_lane` re-runs on runtime Dhan enable/stop→restart/cold-start
   retry (the same per-lane-leak class the DayOhlcTracker 2026-07-01 fix
   addressed) — without the guard every lane restart would leak an extra
   never-exiting supervisor + duplicate gauge writers. First lane start wins;
   later starts log `info!` and skip.
4. New `ErrorCode::Slo03PublisherRespawned` (`code_str "SLO-03"`,
   `Severity::High` → `is_auto_triage_safe() == true`, runbook
   `.claude/rules/project/wave-3-d-error-codes.md`) in
   `crates/common/src/error_code.rs` (+ `all()` list).
5. Runbook section "SLO-03" appended to
   `.claude/rules/project/wave-3-d-error-codes.md` (trigger / triage /
   auto-triage-safe Yes / source) so `error_code_rule_file_crossref` passes.
6. Wiring ratchet `test_slo_publisher_supervisor_is_wired_into_main` in
   `crates/core/src/auth/secret_manager.rs` tests (mirror
   `test_oom_monitor_is_wired_into_main`) — source-scans `main.rs` for
   `spawn_supervised_slo_publisher(`.

Out of scope (deliberate): the tick_freshness de-noise (follow-up B3 lives in
`active-plan-slo-tick-freshness.md`, parallel session); CloudWatch alarm on
the respawn counter (follow-up); any change to `slo_score.rs` semantics.

## Edge Cases

- **Lane restart (D2b runtime toggle):** once-guard prevents duplicate
  supervisors/publishers; the first supervisor keeps running across lane
  stop/start (it reads process-shared `SharedHealthStatus` + `FeedRuntimeState`
  + the token-headroom helper already prefers the LIVE lane manager, so
  values stay correct after a lane restart).
- **Feature flag OFF:** nothing spawns (gate unchanged).
- **Clean exit vs panic vs cancel:** `classify_join_exit` labels
  `clean_exit` / `panic` / `cancelled` / `unknown` — all respawn (the loop is
  infinite; ANY resolution is abnormal).
- **Flapping publisher:** bounded 5s backoff between respawns; the `reason`
  label on `tv_slo_publisher_respawn_total` exposes a storm; each death logs
  `error!` (Telegram via the 5-sink chain).
- **Shutdown:** supervisor task is detached (handle bound `_`-prefixed);
  process teardown aborts it like every other background task — no new
  shutdown path required (same posture as OOM/disk supervisors).

## Failure Modes

- **Inner task dies (the incident):** supervisor logs SLO-03 + increments
  counter + respawns within 5s → metric stream resumes; the alarm false-OK
  window shrinks from "rest of session" to ≤ ~15s.
- **Supervisor itself has no panic path:** body is pure classification +
  clone + sleep (no unwrap/expect), mirroring the OOM supervisor's
  "no supervisor-of-the-supervisor needed" contract.
- **Metrics recorder unavailable:** `metrics::counter!` no-ops; `error!`
  still routes to the 5 sinks.
- **Death-path root cause (investigated, verdict in PR body):** no certain
  in-loop panic found; the leading plausible candidate is
  `boot_probe::wait_for_questdb_ready` constructing a fresh reqwest `Client`
  per probe with a `Client::new()` fallback that PANICS if builder init fails
  (fd/resolver exhaustion — consistent with the 10:35–10:36 WS-GAP-06 storm +
  1,127,801-frame STAGE-C.2b re-injection context). Not fixed here (not
  certain); the supervisor makes any recurrence loud + self-healing.

## Test Plan

- `cargo test -p tickvault-common` — ErrorCode ratchet suite (unique
  code_str, roundtrip, runbook path exists, all() exhaustive, severity,
  crossref both directions, tag-guard).
- `cargo test -p tickvault-core` — new wiring ratchet
  `test_slo_publisher_supervisor_is_wired_into_main` + existing
  `slo_score` unit tests untouched-green.
- `cargo test -p tickvault-app` — main.rs-scoped tests + boot helpers green
  after the extraction.
- `cargo fmt --check` + `cargo clippy --workspace -- -D warnings -W clippy::perf`.

## Rollback

Single squash-merge revert restores the bare `tokio::spawn` inline block.
No schema change, no config change, no wire-format change (the new metric
and ErrorCode are additive). The once-guard static and the two new fns are
self-contained in `main.rs`.

## Observability

- `error!(code = "SLO-03", reason, backoff_secs, ...)` on every publisher
  death → stdout/app.log/errors.log/errors.jsonl/Telegram chain.
- `tv_slo_publisher_respawn_total{reason}` counter (static labels from
  `classify_join_exit`).
- `info!` on supervisor start and on duplicate-spawn skip (lane restart).
- Runbook: `.claude/rules/project/wave-3-d-error-codes.md` §SLO-03.
- Follow-up (not this PR): CloudWatch alarm on a flapping respawn counter,
  mirroring `tv-<env>-disk-watcher-respawn`.

## Plan Items

- [x] Item 1 — ErrorCode::Slo03PublisherRespawned (SLO-03, High)
  - Files: crates/common/src/error_code.rs
  - Tests: existing ratchet suite (test_all_variants_have_unique_code_str, test_all_list_length_matches_catalogue_size, error_code_rule_file_crossref)
- [x] Item 2 — runbook section SLO-03
  - Files: .claude/rules/project/wave-3-d-error-codes.md
  - Tests: every_error_code_variant_appears_in_a_rule_file
- [x] Item 3 — supervised SLO publisher in main.rs (extract + supervisor + once-guard)
  - Files: crates/app/src/main.rs
  - Tests: test_slo_publisher_supervisor_is_wired_into_main
- [x] Item 4 — wiring ratchet
  - Files: crates/core/src/auth/secret_manager.rs
  - Tests: test_slo_publisher_supervisor_is_wired_into_main
