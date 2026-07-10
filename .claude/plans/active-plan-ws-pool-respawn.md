# Implementation Plan: WS-GAP-05 pool-slot supervised respawn (W2 PR #8)

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — Wave-2 coordinator directive, PR #8 of the wave ("treat as a bug fix — docs-vs-code drift where the DOCS are the contract")

## As-found evidence (Step 0 investigation — Verified)

- `crates/core/src/websocket/connection_pool.rs:731` `supervise_pool` drains
  `JoinHandle`s with ERROR log + `tv_ws_pool_respawn_total` but NEVER respawns.
  Its own doc-comment (line 724) says **"Actual respawn-with-fresh-config is
  deferred"** — a deliberate, documented deferral, not an accidental removal.
- `supervise_pool` has exactly ONE call site: `crates/app/src/main.rs:10018`
  inside `teardown_dhan_lane_tasks` — a 5s-bounded SHUTDOWN drain run AFTER
  `abort()`ing every handle. There is NO live-session supervision loop at all.
- `respawn_dead_connections_loop` / `spawn_pool_supervisor_task` (the names in
  `wave-2-error-codes.md` WS-GAP-05 + `disaster-recovery.md`) exist NOWHERE in
  code — docs-only identifiers.
- Git-history verdict: the repo history is grafted (71 commits, root =
  2026-07-04 #1394) — full forensics impossible; **Unknown** whether a respawn
  loop ever existed. The in-code "deferred" note is the best evidence: never
  finished, not removed.
- Production `run()` (connection.rs:866) task-exit paths:
  `Ok(())` on (a) graceful shutdown (flag `shutdown_requested`, accessor
  `is_shutdown_requested()` connection.rs:821), (b) **server Close frame**
  (connection.rs:1932), (c) **stream-end None** (connection.rs:1950 — the
  in-file comment "outer loop reconnects" is STALE; the outer `run()` match at
  1056 returns), (d) WS-GAP-07 frame-channel-Closed (1864);
  `Err(NonReconnectableDisconnect)` (1113 — deliberate anti-storm stop);
  `Err(ReconnectionExhausted)` (1254 — finite budget / no-calendar post-close;
  unreachable in prod: infinite retries + calendar installed). Panic:
  workspace release profile is `panic = "abort"` → a panicked WS task ABORTS
  the process (TICK-FLUSH-01 honesty precedent) — panic-respawn is
  unreachable in release.
- The live gap: a mid-market server Close / TCP FIN kills the slot task
  permanently. The pool watchdog's WS-GAP-09 ride-out then "keeps the
  per-connection reconnect loops running" — but the loop is DEAD, so the
  ride-out no-ops for 15 min, then `process::exit(2)` → full restart
  (~5–20 min of dead feed for a 5-second problem).

## Design

**In-task supervised respawn loop (replace-not-add by construction).** The
respawn loop lives INSIDE the single per-slot task that `spawn_all`
(connection_pool.rs:329) already spawns — never a second task, never a second
socket, never a supervisor that owns handles:

```
tokio::spawn(async move {
    defer_until_market_open_ist(...).await;      // unchanged, once at spawn
    run_supervised_pool_slot(conn).await         // NEW named loop:
})
// loop { t0 = Instant::now(); result = conn.run().await;
//        verdict = classify_pool_task_exit(&result,
//                     conn.is_shutdown_requested(),
//                     conn.live_frame_channel_closed());
//        Terminal → return result;
//        Respawn  → error!(code = WS-GAP-05) + counter
//                   + storm-bounded backoff sleep + continue }
```

- Pure `classify_pool_task_exit(exit, shutdown_requested, channel_closed)
  -> PoolSlotExitVerdict{Terminal, Respawn}`:
  - `shutdown_requested == true` → Terminal (never resurrect a
    torn-down/gracefully-stopped lane)
  - any `Err(_)` → Terminal (`NonReconnectableDisconnect` = deliberate
    anti-storm stop; `ReconnectionExhausted` = configured budget — respawning
    either would defeat a deliberate bound; watchdog genuine-fatal Halt owns
    process-level recovery)
  - `Ok(())` + `channel_closed` → Terminal (WS-GAP-07: consumer dead —
    respawning would churn Dhan connects with zero benefit)
  - `Ok(())` otherwise → **Respawn** (server Close frame / stream-end — the
    unexpected-clean-exit class; THE live gap)
- Pure `compute_pool_respawn_backoff_secs(consecutive_quick_exits) -> u64`:
  `min(5 × 2^n, 300)` — base 5s per the WS-GAP-05 doc contract, 300s cap
  (WS_RATE_LIMIT_BACKOFF_CAP class). A session that survived ≥ 60s
  (`POOL_RESPAWN_STABILITY_RESET_SECS`, WS-GAP-10 stability-window precedent)
  resets the streak — a persistent server-side close loop degrades to one
  connect per 5 min, loud (one WS-GAP-05 ERROR per cycle), never a storm.
- `connection.rs`: one-line `pub(crate) fn live_frame_channel_closed()`
  accessor over `self.frame_sender.is_closed()`.
- NO changes to `run()`, `run_read_loop`, `wait_with_backoff`,
  `SubscribeRxGuard`, subscription builder, parser, `teardown_dhan_lane_tasks`,
  `DhanLaneRunHandles`, `PreLaneAbortGuard`, or the watchdog.

**How each of the 5 interplays is protected:**
1. **SubscribeRxGuard:** the receiver lives on the `Arc<WebSocketConnection>`
   (`subscribe_cmd_rx` slot); the guard reinstalls it on every `run()` exit
   path. The respawned `run()` re-acquires it — same contract as an in-loop
   reconnect. Subscription restoration = `connect_and_subscribe` re-sending
   `self.instruments` (identical to every existing reconnect).
2. **2-WS lock / double-spawn:** structurally impossible — the respawn is a
   serialized re-entry of `run()` inside the SAME task on the SAME slot Arc.
   Handle count, slot count, ownership (lane `ws_handles` Vec, H8 Drop floor,
   PreLaneAbortGuard) all byte-identical. No new spawn site exists.
3. **Lane teardown / resurrect-after-teardown:** teardown aborts the slot
   handle → the WHOLE wrapper (respawn loop included) is cancelled — no
   respawn possible after abort. Graceful path additionally caught by the
   `is_shutdown_requested()` Terminal arm. Cancellation never respawns
   (there is no external observer to misclassify a JoinError).
4. **Rate-limit cooldown (WS-GAP-08):** a respawned `run()` goes through
   `connect_and_subscribe` → the SAME 429 classification + persisted-cooldown
   + `rate_limit_streak` floors as any reconnect. The respawn arm only fires
   on post-connect clean closes; its own expo backoff (5s→300s) bounds
   polite-close storms.
5. **WS-GAP-09 watchdog:** ride-out's assumption ("per-connection reconnect
   loops keep running") becomes TRUE for the clean-exit class instead of
   false. Genuine-fatal classification unchanged (`saw_non_reconnectable`
   still set; we never respawn that arm).

## Edge Cases

- Respawn while feed disabled (PR-E): `run()` re-entry parks at the
  top-of-loop dormant gate — no socket opened.
- Off-hours server Close: respawned `run()`'s own machinery parks it
  (connect failures → ≥3 attempts → WS-GAP-04 sleep-until-open).
- Abort mid-backoff-sleep or mid-run: future dropped; SubscribeRxGuard Drop
  reinstalls the receiver; no respawn.
- Dev-build panic inside `run()`: propagates out of the wrapper task
  (unchanged vs today; teardown drain classifies it). Release: process abort
  + WAL replay — honestly documented, NOT claimed as respawnable.
- Immediate re-Close forever: expo backoff caps at 300s; every cycle emits
  a coded ERROR — loud, bounded, never silent, never a process-restart loop.
- Stability reset: a 60s+ healthy session resets the backoff to 5s so a
  once-a-day polite close always recovers in ~5s.

## Failure Modes

- Classify wrong on a future new `Ok(())` return site → worst case a
  respawn+reconnect where a terminal stop was intended — bounded by the 300s
  backoff cap and loud per-cycle ERROR; ratchet test pins the current arms.
- Backoff arithmetic overflow → `checked_shl`/saturating math, capped.
- `live_frame_channel_closed` false-negative (channel closes AFTER
  classify) → one extra respawn cycle, next exit classifies Terminal.
- Metrics recorder absent (tests) → no-op, no panic.

## Test Plan

- Unit (pure): `classify_pool_task_exit` — all 6 arms (shutdown×2, Err×2,
  Ok+closed, Ok+alive); `compute_pool_respawn_backoff_secs` — base 5,
  doubling, 300 cap, monotone, no overflow at large n.
- Tokio: `run_supervised_pool_slot` terminal-Err (bad URL +
  `reconnect_max_attempts: 1` → returns `ReconnectionExhausted`, no respawn);
  terminal-Ok (feed-disabled conn + `request_graceful_shutdown` → dormant
  gate exits `Ok(())`, wrapper returns — proves no-resurrect-after-shutdown);
  abort-cancels (spawn wrapper on parked dormant conn, abort, join
  `is_cancelled` — proves no-resurrect-after-teardown-abort).
- Structural: `spawn_all` handle count == `connection_count()` (replace-not-
  add baseline); source-scan ratchet pinning that `spawn_all`'s task body
  calls `run_supervised_pool_slot` and the respawn loop classifies via
  `classify_pool_task_exit` (comment-stripped contains-check).
- Full `cargo test -p tickvault-core` green.

## Rollback

Single-commit revert restores the exact prior spawn body (`defer` +
`conn.run()`); no schema, no config, no cross-crate API change. The two doc
files regain their (stale) wording on revert — acceptable, they described
this behavior aspirationally already.

## Observability

- `error!(code = ErrorCode::WsGap05PoolRespawn.code_str(), …)` per respawn
  (tag-guard compliant; routes to errors.jsonl → CloudWatch).
- `tv_ws_pool_respawn_total{reason="unexpected_clean_exit"}` — the FIRST
  live increment site matching the documented counter semantics (existing
  teardown-drain labels unchanged).
- `wave-2-error-codes.md` WS-GAP-05 section gets a dated 2026-07-10 update:
  real mechanism (`run_supervised_pool_slot` in-task loop), honest
  panic=abort envelope, Terminal/Respawn table; `disaster-recovery.md`'s two
  stale `respawn_dead_connections_loop` references corrected.

## Plan Items

- [x] Add `live_frame_channel_closed` accessor
  - Files: crates/core/src/websocket/connection.rs
  - Tests: test_live_frame_channel_closed_tracks_receiver_drop
- [x] Add classify + backoff pure fns, constants, `run_supervised_pool_slot`,
  wire into `spawn_all`
  - Files: crates/core/src/websocket/connection_pool.rs
  - Tests: test_classify_terminal_on_shutdown, test_classify_terminal_on_err,
    test_classify_terminal_on_channel_closed, test_classify_respawn_on_clean_exit,
    test_respawn_backoff_base_doubling_cap, test_supervised_slot_terminal_err_no_respawn,
    test_supervised_slot_terminal_ok_on_shutdown, test_supervised_slot_abort_is_cancelled,
    test_spawn_all_handle_count_equals_connection_count,
    ratchet_spawn_all_uses_supervised_slot_loop
- [x] Doc truth-sync (WS-GAP-05 runbook + disaster-recovery)
  - Files: .claude/rules/project/wave-2-error-codes.md,
    .claude/rules/project/disaster-recovery.md
  - Tests: n/a (docs)
