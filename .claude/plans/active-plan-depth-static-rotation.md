# Implementation Plan: Depth-20 static day set + depth-200 rotate-by-reconnect

**Status:** APPROVED
**Date:** 2026-09-24
**Approved by:** Parthiban (operator) — "See fix and resolve and build and implement everything entilrey dude why stopping dude why"
**Authority:** `.claude/rules/project/websocket-connection-scope-lock.md` § "2026-09-24"
**Crates:** core (`crates/core/src/websocket/pool_supervisor.rs`), app (`crates/app/src/dhan_feed_stack.rs`, `crates/app/src/depth_rebalance.rs`)

## Design

- core: `ReconnectReason::RankedRotation` (flap-exempt like `ProbeClose`),
  `ConnEvent::RotationRequested` (Live-gated redial), and
  `LiveSubscriptionCommand::RotateByRedial{old,new,ack}`. The drain handler:
  requires a one-instrument guard holding `old`, does a guard-only `try_swap`,
  then closes and redials. The redial replays the guard, so it subscribes `new`
  and never sends `old`. Also a process-wide `ROTATION_HALTED` set on the first
  805 (PoolOverflow).
- app depth-200: `send_swap` sends `RotateByRedial` instead of `Swap` and skips
  with a counted reason while `rotation_halted()`. Candidates come from the 3 s
  board until the first publish, then the 1-minute board.
- app depth-20: the static set is every F&O underlying NSE_EQ spot from the daily
  F&O artifact, plus NIFTY/BANKNIFTY options ATM ±k (k = largest ≤ 4 that fits
  250), added at ≥ 09:12. The legs go into the initial dial if the attach runs
  after 09:12. Otherwise they are added by `Extend` on sockets with room. No
  per-minute depth-20 steering.

## Edge Cases

- F&O artifact absent or unreadable → depth-20 dials no spots, logs a coded
  error, and the index legs still go in.
- Spot count so large that k = 0 does not fit → no index legs, counted, never a
  breach of 250.
- A missing index ladder (no candidates) → that index contributes nothing,
  logged.
- A rotation to a contract already on another socket → refused by the planner
  (existing distinct-underlying rule).
- A rotation arriving while the socket is not Live → guard reverted, NotHeld.
- Restart mid-session → index legs recomputed from today's candidates at the
  current price (the day file is deferred; see Rollback).

## Failure Modes

- 805 on any socket → `ROTATION_HALTED` is set, no further rotation, and the
  pool holds its set.
- The redial fails → the normal backoff ladder applies. The guard already names
  `new`, so the retry subscribes `new`.
- `Extend` channel full or closed → counted, retried on the next minute.
- The NSE_EQ depth subscribe is silently ignored by Dhan → the silence detector
  reports never-ticked instruments. There is no crash.

## Test Plan

- core unit tests: `RankedRotation` does not record a flap; the flap-exemption
  test's list becomes `["probe_close","ranked_rotation"]`; `RotationRequested`
  redials only when Live; the halted flag is set in the 805 arm.
- app unit tests: `fno_spot_instruments` parses segment 1 only; `index_k_for`
  picks the largest k ≤ 4 that fits; `static_depth20_set` dedups and never
  exceeds 250; `send_swap` builds `RotateByRedial`.
- Guard test update: depth steering reads the 3 s board until the first publish,
  then 1 m.
- `cargo test -p tickvault-core -p tickvault-app`, clippy `-D warnings`, fmt.

## Rollback

Revert the PR. The old name-board and unsubscribe path return intact because
nothing else is deleted. No schema change, no config change, no data migration.

## Observability

- `tv_depth200_rotations_total{outcome}` (local `/metrics` only — no EMF, no
  alarm, per the budget lever rule).
- `tv_depth20_static_legs{kind}` gauge (local): spots and index legs placed.
- Coded `info!`/`warn!` lines with `source = "ranked_rotation"`,
  `"rotation_halted"`, `"depth20_static"`.
- Existing: `tv_depth_first_packet_latency_ms`, `tv_dhan_feed_depth_total`,
  `REBALANCE_SWAPS_SENT/REFUSED`.

## Plan Items

- [ ] Rule-file section 2026-09-24
- [ ] core: RankedRotation + RotationRequested + RotateByRedial + ROTATION_HALTED + tests
- [ ] app: depth-200 send_swap → RotateByRedial, 3s→1m board
- [ ] app: depth-20 static set + index legs + remove per-minute depth-20 steering
- [ ] tests / clippy / fmt green; PR All Green; merge; deploy after 15:45 IST
