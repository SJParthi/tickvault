# Implementation Plan: Cadence Real Broker Executors (all-7 primary live)

**Status:** DRAFT
**Date:** 2026-07-17
**Approved by:** pending

## Design

Two real broker executors — `crates/app/src/dhan_cadence_executor.rs` and
`crates/app/src/groww_cadence_executor.rs` — implement the
`crates/core/src/cadence` `CadenceExecutor` trait. Each executor calls the
limiter-free `*_unpaced` inner fetchers extracted from the legacy per-minute
legs (extraction already landed; the paced legacy wrappers are unchanged and
keep routing through the shared limiter). The cadence RUNNER — not the
executors — owns the gates, the single bounded in-cycle retry, and the
RateLimited-only shape ladder: all-7-concurrent primary → chains-second-1 /
spots-second-2 split after 2 consecutive rate-limited cycles → recover after
3 consecutive clean cycles, with the rung-0 re-entry cap of 3 per IST day.

Executor responsibilities per fire: fetch + persist (`spot_1m_rest` /
`option_chain_1m`, feed-tagged rows, DEDUP-idempotent re-appends) +
`rest_candle_fold` confirmed-bar handoff strictly AFTER the ILP flush ACK +
chain-snapshot registry publish + `rest_fetch_audit` forensics rows + the
`tv_rest_1m_fire_heartbeat` liveness gauge set on every fire.

`crates/app/src/cadence_boot.rs` constructs the real executors for BOTH lanes
with `dry_run: false` and fire-time token resolution (the token is resolved at
each fire instant, never captured at boot). `config/base.toml` flips
`[cadence] enabled = true` and disables the 4 legacy per-minute leg configs
(`[spot_1m_rest]`, `[option_chain_1m]`, `[groww_spot_1m]`,
`[groww_option_chain_1m]`) per the `AppConfig::validate` mutual exclusion —
the legacy modules stay intact on disk (config-disabled only, no deletion).

## Edge Cases

- HTTP 429 (+ any `Retry-After` header) maps to
  `CadenceFetchError::RateLimited { retry_after_ms }` — the sole
  ladder-arming class.
- A 2xx response WITHOUT the target minute (spot) and a zero-strike chain
  body both map to `Empty` — never `RateLimited`, never ladder-arming.
- Missing token at fire time maps to `Auth` — the fire fails typed; the next
  minute re-resolves.
- The fold handoff happens strictly after the persist flush ACK — a
  persist-failed bar is never handed to `rest_candle_fold` (an unpersisted
  bar must not derive candles).
- Boot before the Dhan token mint: fire-time resolution means early fires
  fail `Auth` and self-heal once the token machinery delivers — no boot
  blocking, no captured-stale-token class.
- Expiry unresolved for an underlying: the chain request is stamped
  `expiry = None` per the day-locked store — never guessed.
- Each request is deadline-bounded and single-shot inside the executor — the
  executor performs NO self-retry; the runner owns the one bounded in-cycle
  retry.

## Failure Modes

- Vendor 429 storm: 2 consecutive dirty cycles demote to the split shape;
  3 consecutive clean cycles recover; the rung-0 re-entry cap (3/day) holds
  the split shape for the rest of the session and prevents all-day flapping.
- QuestDB persist outage: the fetch succeeds but the persist fails — counted
  (`persist_errors` stages) + a named-gap `rest_fetch_audit` row; a later
  DEDUP-idempotent re-append heals the window; no fold handoff occurs for
  the failed bar.
- Chain-snapshot publish failure: counted; the decision path fails closed on
  the stale/absent snapshot (the decision-freshness gate).
- Heartbeat loss (both spot legs dead/disabled): the
  `tv-<env>-market-hours-liveness-missing` alarm pages — the designed loud
  outcome for zero in-session capture.
- A dead executor/runner task: the existing supervised respawn machinery
  (CADENCE-03 `task_respawn`) re-spawns it; release-build panics abort per
  `panic = "abort"`.

## Test Plan

- Executor unit tests: HTTP-status → `CadenceFetchError` mapping incl.
  negatives (429→RateLimited with/without Retry-After; 2xx-empty→Empty never
  RateLimited; 401/403→Auth; timeout→Timeout; transport→Transport);
  persist→fold ordering (no handoff without flush ACK); no-token→Auth.
- Runner-level fallback proof (`crates/core/tests`): RateLimited on 2
  consecutive cycles → demote to the split shape; 3 clean cycles → recover;
  Timeout/Transport/Empty/QueueDelay never demote.
- The existing ladder unit suite (`ladder.rs`) + the
  `cadence_zero_429_replay` proptest (all shape-rung × tier permutations,
  zero spacing violations, combined cap-5 ring).
- `cadence_boot_wiring_guard` updates + config-validate tests:
  cadence-on + all-4-legs-off passes; cadence-on + any-leg-on fails
  (`AppConfig::validate` mutual exclusion, both key directions).
- The RS5 limiter-free source-scan ratchet: no cadence executor routes
  through `dhan_data_api_limiter`.
- Scoped suites: `cargo test -p tickvault-app -p tickvault-core
  -p tickvault-common`.

## Rollback

Config flip back: `[cadence] enabled = false` + re-enable the 4 legacy
per-minute leg configs, then redeploy. All paths are config-gated in both
directions; the legacy modules were never removed. No schema change is
involved — both eras write the same shared tables with the same DEDUP keys,
so rows from either path remain idempotent and queryable either way.

## Observability

- The existing `tv_cadence_*` counters/gauges and the CADENCE-01/02/03
  staged logs are unchanged and now carry real-fire data.
- Executor fires keep `rest_fetch_audit` rows flowing, so the 15:45 IST
  scoreboard REST-leg digest keeps measuring close-to-data latency per
  (feed, leg).
- `tv_rest_1m_fire_heartbeat` is set on every executor fire, keeping the
  market-hours-liveness alarm fed exactly as the legacy legs did.
- Spot persists keep `candles_*`, the RAM stores, and the 15:40 IST
  tf-consistency verifier alive via the `rest_candle_fold` confirmed-bar
  handoff (a dead fold still reads Blind, never NoData).

## Plan Items

- [x] Dhan real executor (`DhanCadenceExecutor` — fetch + persist + fold
      handoff + snapshot publish + audit + heartbeat)
  - Files: crates/app/src/dhan_cadence_executor.rs (tickvault-app)
  - Tests: test_cadence_map_spot_failure_taxonomy,
    test_cadence_map_chain_failure_taxonomy,
    test_cadence_fold_handoff_only_after_flush_ack_source_order,
    test_cadence_no_token_at_fire_time_maps_to_auth
- [x] Groww real executor (`GrowwCadenceExecutor` — same responsibilities,
      Groww endpoints/identity)
  - Files: crates/app/src/groww_cadence_executor.rs (tickvault-app)
  - Tests: test_groww_cadence_map_spot_failure_taxonomy,
    test_groww_cadence_map_chain_failure_taxonomy,
    test_groww_cadence_fold_handoff_only_after_flush_ack_source_order,
    test_groww_cadence_no_token_at_fire_time_maps_to_auth
- [x] Limiter-free `*_unpaced` inner-fetcher extraction from the legacy
      per-minute legs (done — commit 0bccc2ff)
  - Files: crates/app/src/spot_1m_rest_boot.rs,
    crates/app/src/option_chain_1m_boot.rs,
    crates/app/src/groww_spot_1m_boot.rs,
    crates/app/src/groww_option_chain_1m_boot.rs (tickvault-app)
  - Tests: existing leg unit suites (unchanged paced wrappers)
- [x] Fold handoff + heartbeat wiring in both executors (post-flush-ACK
      confirmed-bar send; `tv_rest_1m_fire_heartbeat` per fire)
  - Files: crates/app/src/dhan_cadence_executor.rs,
    crates/app/src/groww_cadence_executor.rs (tickvault-app)
  - Tests: test_cadence_spot_fire_sets_rest_1m_heartbeat_gauge,
    test_groww_cadence_spot_fire_sets_rest_1m_heartbeat_gauge
- [x] Boot wiring: `cadence_boot.rs` constructs real executors for both
      lanes, `dry_run: false`, fire-time token resolution
  - Files: crates/app/src/cadence_boot.rs (tickvault-app)
  - Tests: test_cadence_boot_module_gate_guard_and_real_executors,
    test_cadence_base_toml_enabled_and_legacy_legs_stood_down
- [x] Config flip + validate tests (`[cadence] enabled = true`, 4 legacy leg
      configs disabled; mutual-exclusion validation both directions)
  - Files: config/base.toml, crates/common/src/config.rs (tickvault-common)
  - Tests: test_application_config_validate_cadence_capture_leg_mutual_exclusion
    (both directions: cadence-on + all-legs-off passes; cadence-on + any
    leg on fails)
- [x] Wiring-guard lockstep updates for the new spawn/executor shape
  - Files: crates/app/tests/cadence_boot_wiring_guard.rs (tickvault-app)
  - Tests: cadence_boot_wiring_guard (updated pins)
- [x] RS5 limiter-free source-scan ratchet (no cadence executor routes
      through `dhan_data_api_limiter`)
  - Files: crates/app/tests/cadence_executor_purity_guard.rs (tickvault-app
    — landed as a NEW app-crate guard: the core composition guard reads
    core files only)
  - Tests: test_cadence_executors_never_touch_limiter_or_gates
- [x] Runner-level fallback-proof test (demote/recover cycle semantics,
      non-arming classes)
  - Files: crates/core/tests/cadence_fallback_proof.rs (tickvault-core;
    the RateLimited demote/recover arcs were already runner-pinned in
    cadence_runner_dry_run.rs — inventory in the new file's header)
  - Tests: test_dhan_sustained_timeout_only_cycles_never_demote,
    test_dhan_sustained_transport_only_cycles_never_demote,
    test_dhan_sustained_empty_only_cycles_never_demote,
    test_groww_sustained_timeout_only_cycles_never_demote

Guarantee matrices: per .claude/rules/project/per-wave-guarantee-matrix.md (cross-referenced).
