# Active Plan — Dhan-lane cold-start side-effect race + Drop-guard

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session.
**Branch:** `claude/dhan-lane-coldstart-race`
**Crate:** `tickvault-app` (`crates/app/src/main.rs`)
**Companion rule:** `.claude/rules/project/dhan-lane-error-codes.md` (the FSM H5–H8
contract this fix restores), `.claude/rules/project/websocket-connection-scope-lock.md`
(PR-E runtime-toggle + dry_run-only disable gate — UNCHANGED).

## Design

The runtime Dhan-lane cold-start FSM (D2b) has a sub-second disable-during-start
race that leaks a live WebSocket pool. The cold-start task
(`run_dhan_lane_cold_start`, `Ok(handles)` arm) and the runtime supervisor
(`run_dhan_lane_runtime_supervisor`, `StartCancelled` CAS) BOTH race to advance
the `LaneState` FSM out of `Starting`. The supervisor already does the right
thing (CAS-first: it only aborts + marks-not-running when its
`advance_dhan_lane(StartCancelled)` wins `Starting→Off`). The cold-start task
does NOT: in its `Ok(handles)` arm it runs `advance_dhan_lane(StartSucceeded)`
but then UNCONDITIONALLY runs `mark_dhan_pool_present()`,
`set_dhan_lane_running(true)`, the start-notify, and
`park_running_dhan_lane(ctx, handles)` regardless of whether that CAS returned
`Some(Running)`.

When the operator disables Dhan in the window where `start_dhan_lane` is
returning `Ok`, the supervisor can win `StartCancelled` (Starting→Off). The
task's `StartSucceeded` CAS is then `(Off, StartSucceeded)` → `None` (the FSM is
total and rejects it). But the task's side-effects still run: it sets
`dhan_pool_present=true` + `dhan_lane_running=true` while `dhan_lane_state==Off`
(an inconsistent triple), then `park_running_dhan_lane` observes `desired_on ==
false`, tries `advance_dhan_lane(StopRequested)` from `Off` (illegal → `None`),
falls through to the poll-sleep branch — where the supervisor's `task.abort()`
lands and DROPS `handles: Option<DhanLaneRunHandles>`. `DhanLaneRunHandles` has
NO `Drop` impl, so dropping its `Vec<JoinHandle>` DETACHES the WS-pool tasks
instead of aborting them → a live main-feed WS pool LEAKS while the FSM reads
`Off`. This violates the lane design's H6 (Stopping→Off only after handles join)
and H8 (no leaked pool on cancel).

**The fix (two layers, minimal, matching the FSM design):**

1. **Primary — gate all side-effects on the CAS result.** In the `Ok(handles)`
   arm, run `mark_dhan_pool_present()` / `set_dhan_lane_running(true)` / notify /
   `park_running_dhan_lane(ctx, handles)` ONLY when
   `advance_dhan_lane(StartSucceeded) == Some(Running)`. On the `None` branch
   (supervisor already moved Starting→Off), call
   `teardown_dhan_lane_tasks(handles).await` (graceful RequestCode-12 unsubscribe
   + shutdown_notify + JOIN the just-spawned pool), then `clear_live_token_manager()`,
   then `return` — never park a pool the FSM already moved to `Off`. This mirrors
   the supervisor's confirmed-cancel teardown.

2. **Defense-in-depth — `Drop` for `DhanLaneRunHandles`.** Add a `Drop` impl that
   fires `shutdown_notify.notify_waiters()` then `.abort()`s every `ws_handle` +
   `processor_handle` + `renewal_handle` + `order_update_handle` + `trading_handle`
   it owns — so even an unexpected drop (a future refactor, a panic-unwind through
   `park_running_dhan_lane`, an abort landing on a teardown await) can NEVER
   silently detach a live pool. Drop uses only sync-safe `notify_waiters` +
   `JoinHandle::abort` (both non-blocking, no `.await`, no lock-across-await).

3. **C1 cancel-safety (hostile-review CRITICAL, fixed in this PR).** Adding `Drop`
   makes `DhanLaneRunHandles` non-destructurable (E0509). The adversarial review
   caught that the natural rewrite — `mem::take` every field out of `lane` at the
   TOP of `teardown_dhan_lane_tasks` — RE-INTRODUCED the leak: the lost-CAS branch
   calls `teardown_dhan_lane_tasks(handles).await` and is concurrently `.abort()`ed
   by the supervisor; the teardown's FIRST `.await` is the graceful 2s WS drain
   `sleep`, which is BEFORE the WS abort loop. A mid-sleep cancel dropped the
   moved-out local `ws_handles` Vec un-aborted → detach (the H8 floor on `lane` was
   defeated because the handles had been moved OUT of `lane`). **Fix:** keep
   `ws_handles` (and `processor_handle`/`trading_handle`) INSIDE `lane` across every
   `.await` until the instant they are legitimately consumed; `mem::take` them out
   only AFTER the graceful-drain await, with no `.await` between the take and the
   `supervise_pool` consumption. So a mid-drain cancel drops `lane` → `lane`'s Drop
   aborts them. `renewal_handle`/`order_update_handle` are taken+aborted before any
   await (safe). On normal completion every field is taken, so `lane` drops as a
   no-op (no double-abort).

## Edge Cases

- **Supervisor wins the cancel (the bug):** task's `StartSucceeded` CAS → `None`
  → take the new teardown branch → graceful close + join + `clear_live_token_manager`
  + return. No leak, no phantom-running flags, no false "Dhan started" Telegram.
- **Task wins (normal success):** `StartSucceeded` CAS → `Some(Running)` → the
  existing path runs unchanged (mark present, set running, notify, park). Boot-ON
  inline path is byte-identical.
- **Graceful teardown path (operator disable while Running):**
  `teardown_dhan_lane_tasks` destructures by value → Drop sees a moved-out struct
  → no-op. No double-abort.
- **Watchdog-Halt path:** `handle_lane_watchdog_halt` also calls
  `teardown_dhan_lane_tasks(owned)` (by value) → Drop no-op. Unchanged.
- **Abort lands on the sleep branch with `handles: Some(..)` still owned:** Drop
  fires → notify + abort every handle → pool torn down (defense-in-depth catch).
- **Empty / partial handle sets:** every field is `Option`/`Vec`; Drop iterates
  safely (no-op on `None`/empty).

## Failure Modes

- **Teardown drain hangs on the new `None` branch:** wrapped in the same
  `DHAN_LANE_TEARDOWN_DRAIN_TIMEOUT_SECS` bounded `timeout` used by the disable
  path; on timeout emit `DHAN-LANE-04` (DhanLane04TeardownTimeout) — the inner
  `supervise_pool` force-abort already fired; lane still reaches `Off`.
- **Drop double-fires `notify_waiters` + on already-aborted handles:** both are
  idempotent (notify wakes zero waiters; abort on a finished/aborted handle is a
  no-op). No panic, no deadlock — Drop does NO `.await`, holds NO lock.
- **`clear_live_token_manager` on the new branch:** idempotent (slot may already
  be `None` if the start failed before `set_live_token_manager`).

## Test Plan

- `cargo check -p tickvault-app` — clean compile (the structural fix).
- `cargo test -p tickvault-app dhan_lane` + `cargo test -p tickvault-api feed_state`
  — the FSM totality + source-scan ratchets stay green.
- New source-scan ratchet `runtime_cold_start_gates_side_effects_on_cas` in the
  `main.rs` test module: asserts the `Ok(handles)` arm gates
  `park_running_dhan_lane` behind `== Some(LaneState::Running)` and has a
  `teardown_dhan_lane_tasks(handles)` (or `(handles).await`) on the lost-CAS
  branch — pins the fix against regression.
- New unit test `dhan_lane_run_handles_drop_aborts_handles` (in `main.rs` tests):
  builds a `DhanLaneRunHandles` whose `ws_handles` hold a never-completing spawned
  task, drops it, and asserts the handle is finished/aborted shortly after — proves
  the Drop tears down rather than detaches.
- New source-scan ratchet `teardown_keeps_ws_handles_in_lane_across_the_graceful_drain_await`:
  asserts `mem::take(&mut lane.ws_handles)` appears AFTER the graceful 2s `sleep().await`
  in `teardown_dhan_lane_tasks` — pins the C1 cancel-safety fix against regression.
  (The live disable-during-cold-start race itself needs concurrent live tasks + a real
  WS pool to exercise the mid-sleep cancel; that path is reasoned + ratcheted by the
  ordering guard + the Drop behavioural test, not e2e unit-covered — documented honestly.)

## Rollback

Single-file, additive change. `git revert <sha>` restores the prior behaviour
(the leak). No schema change, no config change, no new dependency, no new pub fn
on the data path. The Drop impl is purely defensive and cannot change the
graceful path (move-out leaves it a no-op).

## Observability

- The lost-CAS branch reuses the existing `DHAN-LANE-04` typed ErrorCode + the
  `tv_dhan_lane_teardown_forced_total` counter on a bounded-drain timeout — same
  surface as the disable teardown. No new metric needed.
- An `info!` line on the lost-CAS branch ("[dhan-lane] cold-start lost the
  Starting→Off race to the supervisor — tearing down the just-spawned pool, not
  parking") makes the race observable in `errors.jsonl` / journald.
- No new false-OK signal: the start-success notify + `mark_dhan_pool_present` no
  longer fire on the lost-CAS branch, so the operator never sees a phantom "Dhan
  started" for a lane the FSM moved to `Off`.

## Per-Item Guarantee Matrix (15-row + 7-row)

This plan carries the full 15-row "100% everything" matrix and the 7-row
Resilience Demand matrix from
`.claude/rules/project/per-wave-guarantee-matrix.md` (all rows apply verbatim).
Highlights: testing = new source-scan ratchet + Drop unit test (the
concurrency race is reasoned + guard-pinned, not e2e — honestly flagged);
audit coverage = reuses the existing `DHAN-LANE-04` code (no new table); O(1)
axis = N/A — cold control-plane, the hot tick path is UNTOUCHED; uniqueness/dedup
= unaffected (no storage key change); resilience = the fix CLOSES a tick-loss /
pool-leak path (it removes a detach, it adds none); security = no secret surface,
no new input; the 2-Dhan-WS lock + PR-E runtime-toggle + dry_run-only disable gate
all hold (no new WS endpoint, no new connection).

## Honest envelope (charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: the
> runtime Dhan-lane cold-start now gates its mark-present / set-running / notify /
> park side-effects on the `advance_dhan_lane(StartSucceeded)` CAS winning
> `Starting→Running`; on losing that CAS to the supervisor's `StartCancelled`
> (Starting→Off) it gracefully closes + JOINS the just-spawned WS pool via the
> bounded `teardown_dhan_lane_tasks` instead of parking + leaking it. A `Drop` impl
> on `DhanLaneRunHandles` is the defense-in-depth floor — any unexpected drop fires
> the shutdown notify + aborts every owned handle (sync-safe, no `.await`, no lock),
> so a live pool can NEVER silently detach. The pure 5-state `LaneState` FSM
> totality, the new CAS-gating, and the Drop teardown are ratcheted by deterministic
> source-scan + a Drop unit test. The disable-during-cold-start race remediation is
> reasoned against the FSM + the supervisor's CAS-first ordering and PINNED by guards
> — it is NOT claimed to be exercised by a live concurrent e2e test (honestly flagged).
> Boot-ON + the normal success path are byte-identical. The 2-Dhan-WS lock, the PR-E
> runtime-toggle, and the dry_run-only disable gate all hold."
