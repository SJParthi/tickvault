# Implementation Plan: PR-E — Dhan runtime on/off toggle (fully disconnect)

**Status:** APPROVED
**Date:** 2026-06-21
**Approved by:** Parthiban (verbatim 2026-06-21 "if I want to switch off or on dhan also it should be accepted right dude" + AskUserQuestion answer "Fully disconnect Dhan")
**Authority for touching locked code:** `.claude/rules/project/websocket-connection-scope-lock.md` "DHAN RUNTIME-TOGGLE AUTHORIZED 2026-06-21 (PR-E)" header (dated quote recorded there FIRST, as the lock protocol requires).

## Design

Make Dhan a live ON/OFF feed (like Groww), without changing the 2-connection lock,
endpoints, or universe. OFF = fully disconnect the Dhan WS(es) + stop storing; ON =
reconnect + re-subscribe (reuse the existing `SubscribeRxGuard` + dormant-reconnect
machinery).

Layering: the runtime flag (`FeedRuntimeState`) lives in `api`, but the Dhan WS lives
in `core` (a lower layer that cannot depend on `api`). Bridge with a primitive:
- `FeedRuntimeState` inner flags become `Arc<AtomicBool>` (today plain `AtomicBool`);
  add `pub fn dhan_flag(&self) -> Arc<AtomicBool>` returning a clone of the Arc.
- `crates/app/src/main.rs` boot: after building `feed_runtime`, pass
  `feed_runtime.dhan_flag()` (an `Arc<AtomicBool>`) into the WS pool builder at BOTH
  boot-path spawn sites.
- `core` `WebSocketConnectionPool::new*` + `WebSocketConnection` accept an optional
  `Option<Arc<AtomicBool>> dhan_enabled` (None = always-enabled → every existing test
  + the order-update path unaffected unless wired). The main-feed connection read/
  reconnect loop checks it: when `false` → close the socket + enter the existing
  dormant wait (poll the flag every N secs, same pattern as WS-GAP-04 post-close
  sleep); when it flips back to `true` → break the dormant wait → reconnect +
  re-subscribe. The order-update connection takes the SAME flag (Dhan OFF closes both).
- "stop storing" falls out for free: disconnected socket → no frames → no persistence.

API + webpage:
- `Feed::Dhan.is_runtime_toggleable()` → `true`.
- `handlers/feeds.rs` `set_feed`: accept `dhan`; SAFETY GATE — refuse to DISABLE Dhan
  (return 409/400 with a clear reason) unless the no-orders condition holds
  (`dry_run` true / no open orders+positions). Enabling Dhan is always allowed.
- `feeds_page.rs`: Dhan descriptor `toggleable: true`.
- `FeedStatus` gains `dhan_lane_running` honesty mirror (optional; the supervisor/
  pool sets it) so the page never lies.

## Edge Cases
- Toggle Dhan OFF then ON rapidly: dormant wait polls the flag; reconnect is
  idempotent (SubscribeRxGuard re-subscribes; Dhan ignores duplicate subscribes).
- Dhan OFF at boot (`dhan_enabled=false` in config): pool builds but connections
  start dormant (never connect until enabled) — no wasted socket.
- Toggle during market-close dormant sleep: the flag check composes with the
  existing post-close gate (whichever says "stay dormant" wins).
- Groww-only run (`dhan_enabled=false` + `groww_enabled=true`): Dhan connections
  dormant, Groww streams — the operator's primary use case.
- Order-update WS: OFF disconnects it too (no order events while Dhan off — fine in
  no-orders phase; the safety gate blocks OFF once orders are live).
- None flag (all existing call sites / tests): behaves exactly as today.

## Failure Modes
- Flag read races: `Relaxed` atomic; a toggle is observed on the next loop tick
  (advisory, same as Groww). No barrier needed.
- Reconnect storm on rapid toggling: the dormant poll interval (reuse a constant,
  e.g. 2s) bounds reconnect frequency.
- Safety-gate false-block: if the no-orders signal is unavailable, FAIL CLOSED
  (refuse to disable Dhan) — safer to keep the feed than to blind the system.
- Re-enable but socket can't connect: existing reconnect backoff + watchdog applies;
  operator sees the normal WS disconnect/reconnect telemetry.

## Test Plan
- `feed_state.rs`: `is_runtime_toggleable(Dhan) == true`; `dhan_flag()` shares the
  same atomic that `set_enabled(Dhan, ..)` flips (flip via API → flag observed).
- `handlers/feeds.rs`: enabling Dhan ok; disabling Dhan REJECTED when orders-live
  gate is closed; allowed when open (dry_run). Update the existing
  `test_set_feed_dhan_is_rejected_400` to the new gated semantics.
- `feeds_page.rs`: Dhan descriptor is `toggleable: true`.
- `core` connection: pure-logic test for the "dormant while flag false, wake on
  true" decision (extract a `should_stay_dormant(flag, market_gate)` pure helper and
  unit-test it; the socket I/O around it is TEST-EXEMPT, mirroring existing
  reconnect-helper test patterns).
- Workspace `cargo test` green (common change → escalate per testing-scope).

## Rollback
- Single feature, additive: the `Option<Arc<AtomicBool>>` defaults to None
  (always-enabled). Reverting the PR restores config+restart-only Dhan with zero
  data-model change. The `[feeds] dhan_enabled=true` default is unchanged, so a
  revert leaves prod behaviour identical.

## Observability
- `tv_feed_runtime_toggle_total{feed="dhan",action}` (existing counter covers it).
- `dhan_lane_running` in `GET /api/feeds` + the webpage honesty note.
- Dhan OFF→disconnect reuses existing `WebSocketDisconnected*` events (Severity::Low
  off-hours); ON→reconnect reuses `WebSocketReconnected` / `WebSocketPoolOnline`.
- A toggle log line `error!`/`info!` with the feed + action (existing in set_feed).

## Plan Items
- [x] PR-E — Dhan runtime on/off (fully disconnect), safety-gated, webpage live switch. Implemented: `core` `WebSocketConnection` gains `Option<Arc<AtomicBool>>` feed-enable flag (`with_feed_enable_flag`/`feed_enabled`) + top-of-`run()`-loop dormant gate (`wait_until_feed_enabled`) + a read-loop `select!` disable arm returning `WebSocketError::FeedDisabled` (outer match `continue`s to dormant, skipping backoff); pool threads the flag; order-update WS gets a top-of-loop dormant gate. `feed_state` flags are `Arc<AtomicBool>` with `dhan_flag()`; Dhan `is_runtime_toggleable()`→true; `dhan_disable_allowed` safety gate seeded from `dry_run`; `dhan_lane_running` honesty (marked only when dhan_enabled at boot). Handler accepts `dhan`, refuses DISABLE when live trading (409), warns on enable-without-lane. Webpage Dhan `toggleable:true`. main.rs wires the flag at both boot paths + both order-update spawns. Adversarial review: hot-path CLEAN (zero per-frame alloc), hostile HIGH (phantom lane) + 2 MED fixed. Tests: `test_feed_enable_flag_gates_feed_enabled` (core), `test_dhan_flag_shares_the_same_atomic_the_toggle_flips`, `test_dhan_disable_safety_gate`, `test_dhan_lane_running_marker`, `test_both_feeds_runtime_toggleable_after_pr_e`, handler dhan disable/enable/gated (api). 2149 core + 31 api green.
  - Files: `crates/api/src/feed_state.rs`, `crates/api/src/handlers/feeds.rs`,
    `crates/api/src/handlers/feeds_page.rs`, `crates/core/src/websocket/connection.rs`,
    `crates/core/src/websocket/connection_pool.rs`,
    `crates/core/src/websocket/order_update_connection.rs`, `crates/app/src/main.rs`,
    `.claude/rules/project/websocket-connection-scope-lock.md` (done),
    `.claude/rules/project/operator-charter-forever.md` §I (note)
  - Tests: feed_state dhan-toggle + dhan_flag share; feeds handler gated-disable;
    feeds_page dhan toggleable; core dormant-decision pure helper.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Webpage flip Dhan OFF (no orders) | Dhan WS closes, ticks stop; ON → reconnect+resubscribe |
| 2 | Flip Dhan OFF while orders live | Rejected with clear reason (safety gate) |
| 3 | Groww-only run (dhan off, groww on) | Dhan dormant, Groww streams |
| 4 | Rapid OFF/ON | Bounded reconnect; no storm; subscriptions preserved |
| 5 | Existing call sites (None flag) | Identical to today (always-enabled) |
