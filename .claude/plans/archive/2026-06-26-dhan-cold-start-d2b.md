# Implementation Plan: D2b — runtime Dhan cold-start FSM

**Status:** APPROVED
**Date:** 2026-06-26
**Approved by:** coordinator (D2 design `cfd27ca6:.claude/plans/active-plan-dhan-cold-start-d2.md` §0.5 folded in)
**Branch:** `claude/d2b-runtime-fsm`
**Crates touched:** `crates/common`, `crates/api`, `crates/app`

> Builds on the merged D2-pre (shared-infra hoist) + D2a (`start_dhan_lane`
> extraction). D2b makes a Dhan feed that was **OFF at boot** runtime-startable
> via the dormant activation watcher, driving a 5-state `LaneState` FSM with
> every §0.5 safety fix. Defers the full `dhan_lane_audit` table + typed
> Telegram event variants to D2c (documented honest residual — NOT a skeleton;
> every shipped fn has a real call site + real work, Rule 14).

## Design

The runtime cold-start is owned by a NEW supervisor
(`run_dhan_lane_runtime_supervisor`, `crates/app/src/main.rs`) spawned
unconditionally at boot — the Groww-watcher shape: one owned abortable
`JoinHandle`. It idles (zero Dhan work) until `main()` publishes an owned
`DhanLaneRuntimeContext` into a shared `OnceCell` right after
`build_shared_infra` (which runs in BOTH the Dhan-ON and Dhan-OFF paths), then,
on desired-ON + confirmed-empty `LaneState::Off`, spawns `run_dhan_lane_cold_start`.

The cold-start task drives the FSM `Off→Starting`, calls the extracted
`start_dhan_lane`, and on `Ok` drives `Starting→Running` (marking pool-present +
lane-running ONLY here — phantom-running closed), then parks
(`park_running_dhan_lane`) idling until the operator disables Dhan, at which
point it re-asserts the disable gate (H5), tears the lane down via
`teardown_dhan_lane_tasks` (the lane-scoped teardown, C3), awaits the handle
join (H6), and drives `Stopping→Off`. On `Err` it drives `Starting→Off`, emits a
typed `DHAN-LANE-0N` ErrorCode, and retries with the Groww `min(10*2^n,300)s`
backoff.

The FSM is a pure, total `next_lane_state(state, event) -> Option<LaneState>` in
`crates/api/src/feed_state.rs`, stored as an `AtomicU8` in `FeedRuntimeState`
(M9 — the boot-ON inline start AND the runtime supervisor read/write the SAME
state). `ApplicationConfig` gains `Clone` (`crates/common/src/config.rs`) so the
runtime context can hold an owned `Arc<ApplicationConfig>` snapshot.

## Edge Cases

- Rapid ON→OFF→ON flap faster than the 2s poll → level-triggered supervisor
  converges to the final desired value; the CAS in `advance_dhan_lane` prevents
  a double transition.
- ON while already `Running` → FSM rejects `Running→Starting` (idempotent).
- OFF mid-`Starting` (cancel) → supervisor `.abort()`s the cold-start task; its
  `start_dhan_lane` cleanup aborts every lane-owned handle (H8); FSM
  `Starting→Off`.
- Boot-ON → inline spine drives the FSM to `Running`; the supervisor's first
  tick sees a non-`Off` lane and does NOTHING (byte-identical boot).
- Order opens DURING teardown → `park_running_dhan_lane` re-asserts
  `can_disable_dhan()` before the WS-close; gate-closed → bumps
  `tv_dhan_lane_disable_aborted_total` + stays `Running` (H5).
- Start requested while `Stopping` → FSM rejects `Stopping→Starting` (H6).
- Dead cold-start task (panic mid-start) → supervisor resets a stuck `Starting`
  to `Off` + emits `DHAN-LANE-03` so a clean respawn is legal (WS-GAP-05-style).

## Failure Modes

- Universe-build fails → `DHAN-LANE-01` (High), FSM `Starting→Off`, bounded
  retry; cross-link `INSTR-FETCH-*`.
- WS-pool spawn fails → `DHAN-LANE-02` (High); partial lane handles aborted by
  `start_dhan_lane` (no orphan sockets).
- Auth / IP / static-IP / dual-instance gate fails → `DHAN-LANE-03` (High); at
  RUNTIME this NEVER `process::exit`s (unlike boot); FSM `Starting→Off`.
- Teardown drain timeout → `DHAN-LANE-04` (Medium); handles force-aborted, lane
  STILL reaches `Off` (degraded-but-recovered, not data loss).
- A failed start NEVER leaves a half-running lane and NEVER tears down the
  PROCESS seal-writer / aggregator / API server (C2).

## Test Plan

- FSM unit tests (16) in `feed_state.rs`: every legal + illegal transition,
  `next_lane_state_is_total`, byte round-trip + unknown-fail-closed, the atomic
  `advance_dhan_lane` CAS path (legal/illegal/gate-reclose).
- main.rs pure-helper + wiring ratchets (10): `dhan_lane_retry_backoff_secs`,
  `classify_start_lane_error` (typed codes + secret-free reasons),
  `should_spawn_cold_start`, `should_abort_cold_start_on_disable`,
  `lane_state_label`, phantom-running closure, gate-hold (H5), double-pool (H6),
  C2 ownership (runtime region must not spawn PROCESS infra), boot-wiring,
  `runtime_toggle_never_enters_fast_arm` (L10).
- ErrorCode guards: `error_code_rule_file_crossref`, `error_code_tag_guard`,
  `triage_rules_full_coverage_guard`, catalogue-size + prefix-pattern tests.
- Commands: `cargo test -p tickvault-app -p tickvault-common -p tickvault-api`,
  `cargo build` (0 warnings), `cargo fmt --check`, banned-pattern + pub-fn
  guards. Live cold-start exercised by the boot-deploy follow.

## Rollback

Each artefact is `git revert`-able. The runtime path is additive: with the
supervisor spawned but the lane left `Off` (no operator enable of a boot-OFF
Dhan feed) the system behaves exactly as before D2b — boot-ON is byte-identical
(the inline spine drives the FSM; the supervisor no-ops). Reverting the
`crates/api/feed_state.rs` FSM + the `main.rs` D2b section + the
`ApplicationConfig: Clone` derive restores the D2a state. The D2c residual
(`dhan_lane_audit` table, typed Telegram events, lane-owned TokenManager handle)
is independently addable.

## Observability

- Gauge `tv_dhan_lane_state` (0=Off/1=Starting/2=Running/3=Stopping) set ONLY on
  a real FSM transition (edge-triggered, L12).
- Counters `tv_dhan_lane_transitions_total{from,to}`,
  `tv_dhan_lane_start_failed_total{stage}`,
  `tv_dhan_lane_teardown_forced_total`, `tv_dhan_lane_disable_aborted_total`.
- `error!(code = ErrorCode::DhanLane0N...)` for the 4 typed codes (tag-guard
  enforced); `reason` is a FIXED enum discriminant, never a raw auth body.
- Telegram start/stop pings via `NotificationEvent::Custom` (typed variants +
  the `dhan_lane_audit` QuestDB table deferred to D2c).
- Runbook `.claude/rules/project/dhan-lane-error-codes.md` (cross-ref + triage
  rules for all 4 codes).

## Per-Item Guarantee Matrix (15-row + 7-row)

This plan carries the full 15-row "100% everything" matrix and the 7-row
Resilience Demand matrix from
`.claude/rules/project/per-wave-guarantee-matrix.md` (all rows apply verbatim).
Highlights: audit coverage = `dhan_lane_audit` deferred to D2c (the 4 typed
ErrorCodes + counters + gauge are the D2b observability surface); O(1) axis =
N/A (cold control-plane, the hot tick path is untouched); uniqueness/dedup =
unaffected (no new storage key); resilience = WAL/ring/spill/DLQ +
`SubscribeRxGuard` reused unchanged via `start_dhan_lane`; the 2-Dhan-WS lock +
PR-E runtime-toggle + dry_run-only disable gate all hold.

## Honest envelope (charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: D2b
> makes a boot-OFF Dhan feed cold-startable at runtime via the dormant watcher,
> reusing the SAME `start_dhan_lane` extraction + WAL→ring→spill→DLQ +
> `SubscribeRxGuard` machinery (no new WS endpoint — the 2-Dhan-WS lock holds).
> The 5-state `LaneState` FSM is pure-unit-tested for totality; the
> cancel-safety (H8), gate-hold (H5), double-pool-prevention (H6),
> phantom-running closure, and C2 ownership cases are ratcheted by deterministic
> source-scan + pure-helper guards. A failed cold-start returns the lane to
> `Off` with a typed `DHAN-LANE-0N` ErrorCode + bounded `min(10*2^n,300)s`
> retry — NEVER a half-running lane, NEVER tearing down PROCESS infra. The
> disable gate is re-asserted across teardown so the feed can never be blinded
> mid-trade. Boot-ON is byte-identical. NOT claimed: the full `dhan_lane_audit`
> table / typed Telegram events / lane-owned-TokenManager-handle (deferred to
> D2c), nor the fast-arm unification (D2d) — a runtime toggle is guard-tested to
> NEVER enter the fast arm."
