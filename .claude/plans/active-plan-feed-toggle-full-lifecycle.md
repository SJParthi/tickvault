# Implementation Plan: Feed toggle = full lifecycle (cold-start/teardown live) + persist

**Status:** APPROVED (operator 2026-06-24: "yes go ahead and fix and implement
everything" — the webpage switch must START the entire feed live + the choice must
STICK across restart)
**Date:** 2026-06-24
**Blocked-by:** PR #1196 merged (serial-PR lock satisfied).

## The problem (verified, no illusion)
Today the webpage toggle is **pause/resume only** and **does not persist**:
- Boot spawns a feed's lane ONLY inside `if feeds.<x>_enabled` (`main.rs:277` Groww,
  `:261` Dhan). `mark_groww_lane_running()` (`:430`) runs ONLY there.
- If a feed was OFF at boot, NO lane exists → the toggle flips an `AtomicBool` with
  nothing behind it → "DEGRADED · not started at boot" (`feed_state.rs` + `feeds.rs:150`).
- The toggle is a runtime `AtomicBool` (`feed_state.rs`) — it never writes config, so a
  restart re-reads `config/base.toml groww_enabled=false` and reverts.

## Honest scope (this is a multi-PR refactor, NOT a 1-liner)
Cold-starting a feed at runtime means making each feed's **entire boot
sub-sequence callable on demand**: Dhan = auth → daily-universe build → WS pool
spawn; Groww = auth smoke → watch-list build → bridge → Python sidecar supervisor.
Today those are inlined in the linear boot. They must be extracted into
`start_<feed>_lane()` / `stop_<feed>_lane()` that the supervisor calls.

## Design (the "one switch = whole feed" model)
- **A — on-demand per-feed supervisor (core of the fix).** At boot, ALWAYS spawn a
  dormant supervisor task per feed (regardless of the enabled flag). Each supervisor
  loops on `tokio::select!`(flag-changed `Notify` + shutdown):
  - flag ON  → `start_<feed>_lane()` (cold-start auth/instruments/connect/sidecar)
  - flag OFF → `stop_<feed>_lane()` (graceful teardown: close WS, kill sidecar child,
    drop tasks) — idempotent both ways.
  This PRESERVES the #1192 guarantee ("OFF feed touches nothing"): a dormant
  supervisor that has not started does ZERO auth/instrument/connect work — it only
  parks on the flag. #1192's guard is updated from "no task spawned" to "no
  auth/instrument/connect work occurs while OFF" (the stronger, true invariant).
- **B — persist the toggle.** Write the runtime feed-state to a SEPARATE
  `data/feed-state.json` (NOT git-tracked `config/base.toml` — an API endpoint must
  never rewrite the operator's locked config file). Boot reads `base.toml` as the
  default, then overlays `feed-state.json` if present. So the last webpage choice
  survives restart. The write path is behind the existing GAP-SEC-01 bearer auth
  (POST /api/feeds/{feed}) + path-pinned + atomic-write (tmp+rename).

## §32 / lock interactions (must honor)
- Groww live producer = the Python `growwapi` sidecar (`groww_sidecar_supervisor.rs`).
  `stop` must SIGTERM the child + reap; `start` must (re)spawn it. Already auto-managed
  at boot — extract its start/stop into the lane functions.
- Dhan disable safety gate (`dhan_disable_allowed`, websocket-scope-lock PR-E): OFF for
  Dhan still refused while live trading is on. Supervisor must honor it.
- 2-WS Dhan lock unchanged; Groww default-OFF unchanged (config default stays false;
  persistence is an explicit operator action).

## Plan Items (multi-PR — each its own serial PR + 15+7 matrix + 3-agent review)
- [x] **PR-1 (A, Groww first — lower risk, default-OFF):** dormant `run_groww_activation_watcher`
  (+ bridge + sidecar supervisor) spawned UNCONDITIONALLY at boot; the level-triggered
  reconciler cold-starts the lane on enable (ensure tables → auth smoke → watch-list build →
  mark running) and `abort()`s the owned task on disable. OFF-feed isolation preserved as
  self-idle. `groww_activation.rs` + `set_groww_lane_running` + #1192 guard updated to the
  "no work while OFF (dormant poll)" invariant. (Files: crates/app/src/groww_activation.rs,
  crates/app/src/main.rs, crates/app/src/lib.rs, crates/api/src/feed_state.rs,
  crates/app/tests/per_feed_boot_isolation_guard.rs. Tests: test_reconcile_lane_action_*,
  test_set_groww_lane_running_toggles_both_ways, groww_lanes_spawn_dormant_and_self_idle_on_the_enable_flag.)
- [ ] **PR-2 (A, Dhan):** extract `start_dhan_lane`/`stop_dhan_lane` (auth+universe+WS
  pool); dormant supervisor; honor `dhan_disable_allowed`. (Bigger — Dhan boot is the
  linear spine.)
- [ ] **PR-3 (B, persist):** `data/feed-state.json` overlay (atomic write on toggle,
  boot overlay-read); GAP-SEC-01 protected; ratchet test.
- [ ] **PR-4 (guards):** update the #1192 boot-isolation guard to the "no work while OFF"
  invariant; add cold-start/teardown integration + chaos tests (toggle storm, sidecar
  crash-on-start, auth-fail-on-cold-start, double-toggle idempotency).

## Edge cases / worst cases (must cover)
- Toggle ON then OFF before cold-start finishes (cancel-safe).
- Cold-start auth fails → lane reports DEGRADED with reason, supervisor retries per policy.
- Sidecar `pip install` / spawn fails → DEGRADED, not a crash.
- Double ON / double OFF → idempotent.
- Dhan OFF refused while orders live (safety gate).
- Restart with feed-state.json present → last choice restored; corrupt json → fail-safe to base.toml default.

## Failure Modes
- **Toggle storm (ON→OFF→ON faster than the 2s poll):** the reconciler is
  level-triggered (compares DESIRED vs ACTIVATED, never PREV vs NOW), so a sub-poll
  flap converges to the final state — no missed edge, no leaked task. Proven by
  `test_reconcile_lane_action_sub_poll_flap_converges_no_missed_edge`.
- **In-flight activation when disabled:** the watcher owns ONE `JoinHandle`; on the
  disable transition it `.abort()`s it, cancelling the inline auth-check + watch-list
  build at their next await — no Groww server touched after OFF, no leaked build loops.
- **False-OK (lane reports running while empty):** `mark_groww_lane_running()` is
  called ONLY after the watch-list build's Ok arm; on disable the flag is cleared.
  Positionally pinned by the #1192 guard (mark must follow the watch-list-ready log).
- **Permanent-error infinite loop:** an invalid `watch_date` is a permanent error;
  a fail-closed `is_valid_trading_date` guard at activation start bails instead of
  spinning the pull-until-success loop forever.
- **QuestDB unreachable at ensure-tables / Groww auth fails:** logged at ERROR
  (routes to Telegram); the watch-list build still retries (pull-until-success); the
  lane stays not-running until the watch-list actually builds (honest DEGRADED).
- **OFF feed isolation breach:** the bridge + sidecar supervisor + activation watcher
  all self-idle on `is_enabled(Feed::Groww)`; an OFF feed does only a 2s `AtomicBool`
  poll — no auth, no instruments, no Python, no network.

## Test plan
- Per-crate unit + integration; toggle-lifecycle chaos tests (PR-4); `dhat` (no hot-path
  alloc — the supervisor is cold path); fmt + clippy -D warnings; 3-agent adversarial
  review per PR. PR-1 shipped: 6 `reconcile_lane_action` unit tests + the
  `set_groww_lane_running` round-trip test + the 5-assertion boot-isolation guard
  (`groww_lanes_spawn_dormant_and_self_idle_on_the_enable_flag`) proving unconditional
  dormant spawn, self-idle, owned-JoinHandle+abort lifecycle, and no-false-OK ordering.

## Rollback
- Feature-flagged supervisor; revert per PR. Persistence overlay is additive (delete the
  json → pure base.toml behaviour).

## Observability
- Per-feed lane state gauge (dormant/starting/running/stopping/degraded) + the existing
  feed-page status; ERROR on cold-start failure.

## Per-Item Guarantee Matrix
See `.claude/rules/project/per-wave-guarantee-matrix.md` — all 15 rows of the
100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to
EVERY item (PR-1..PR-4) in this plan: 100% code/audit/testing coverage, code
checks, performance, monitoring, logging, alerting, security + hardening, bug
fixing, scenario + functionality coverage, code review, extreme check; and the
7 resilience rows (zero ticks lost, WS reconnect, never slow/locked, QuestDB
absorb, O(1) hot path UNTOUCHED, composite-key uniqueness + dedup, real-time
proof). Each PR carries the honest envelope wording ("100% inside the tested
envelope, with ratcheted regression coverage") and runs the 3-agent adversarial
review.

## Honest envelope
- O(1) is NOT the relevant axis here — this is the COLD control-plane (toggle/boot),
  not the hot tick path. The hot path (already O(1)) is untouched. Claiming "O(1)" for a
  feed cold-start would be an illusion; the honest guarantee is "starts/stops the full
  lane live, idempotent, fail-safe, no restart."
