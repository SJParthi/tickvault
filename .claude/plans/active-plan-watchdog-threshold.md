# Implementation Plan: DailyUniverse activity-watchdog floor 3s → 15s (audit GAP-1)

**Status:** APPROVED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator, 2026-07-02) — verbatim dated quote: `Operator approved 2026-07-02: "approve 15s"` (supersedes the operator-locked 2026-05-13 3s clamp for the DailyUniverse arm)

> Guarantee matrices: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
> (15-row + 7-row) — this is a single-constant safety fix in `tickvault-core`
> + its `tickvault-app` consumer; the applicable rows are Testing coverage
> (unit ratchet), Code checks (all pre-commit gates), WS-never-disconnects
> envelope (this fix REMOVES a self-inflicted reconnect path), Extreme check
> (build-failing ratchet test pins 15).

## Design

`crates/app/src/main.rs` clamps `activity_watchdog_threshold_secs` per
`SubscriptionScope`. The `DailyUniverse` arm reused
`WATCHDOG_THRESHOLD_IDX_I_SECS = 3` (`crates/core/src/websocket/activity_watchdog.rs`).
Dhan's server pings every 10s and the read loop (`connection.rs` STAGE-C.3)
bumps the activity counter on EVERY frame including pings, so a healthy
socket always shows activity within 10s. A 3s threshold therefore
force-reconnects a HEALTHY socket during any ≥3s data lull inside the
market-hours gate (thin pre-open 09:00–09:15) — violating the module's P2.1
intent and the 50s config default (40s server timeout + 10s margin).

Fix: add `WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS: u64 = 15` (one full ping
interval + 50% margin — can never fire on a healthy socket; detects a truly
silent socket 3.3x faster than the 50s default) and point the DailyUniverse
clamp arm at it. The Indices4Only arm keeps its historical 3s value
untouched. Update the stale comments in `main.rs` and `connection.rs`, add a
dated section to `.claude/rules/project/live-market-feed-subscription.md`.

## Edge Cases

- Thin pre-open 09:00–09:15 with zero data frames: pings every 10s keep the
  counter moving; 15s never fires (was: guaranteed false-fire at 3s).
- Truly silent socket (no frames at all, not even pings): fires at 15s —
  faster than the 50s default, slower than nothing-detected.
- Poll cadence: `WATCHDOG_POLL_INTERVAL_SECS = 5` gives exactly 3 sample
  windows inside 15s (ratcheted `poll * 3 <= threshold`).
- Indices4Only scope: unchanged 3s (historical arm untouched).
- Config override: the clamp still only replaces the configured value when
  it differs; the 50s TOML default remains the non-clamped baseline.

## Failure Modes

- If Dhan lengthens its ping interval beyond 15s, the threshold could fire
  on a healthy socket → the existing reconnect path (SubscribeRxGuard,
  0ms first retry) absorbs it exactly as today at 3s, but 5x less often;
  the ratchet test documents the 10s cadence assumption for re-review.
- If the constant regresses to ≤10s, the new ratchet test
  `daily_universe_threshold_is_15_above_dhan_ping_cadence` fails the build.
- Reconnect storms from real Dhan-side RSTs are untouched (WS-GAP-09
  ride-out logic is independent of this threshold).

## Test Plan

- New unit ratchet in `activity_watchdog.rs::tests`:
  `daily_universe_threshold_is_15_above_dhan_ping_cadence` — pins 15,
  `> 10` (ping cadence bound), `< WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS`,
  and `WATCHDOG_POLL_INTERVAL_SECS * 3 <= 15`.
- Existing `phase_0_watchdog_thresholds_match_plan_spec` still pins
  IDX_I = 3 (Indices4Only historical arm unchanged).
- `cargo test -p tickvault-core --lib websocket::activity_watchdog` +
  `cargo check -p tickvault-app` + `cargo fmt --check`.

## Rollback

Single-commit revert on branch `claude/watchdog-threshold-15s` (not pushed;
awaiting operator approval). Reverting restores the 3s clamp exactly —
config surface, TOML schema, and every other scope arm are untouched.

## Observability

- The existing boot `info!("Phase 0 Item 4: main-feed activity-watchdog
  threshold clamped by scope", configured_secs, effective_secs)` now logs
  `effective_secs = 15` under DailyUniverse — the change is visible at every
  boot.
- Watchdog fires still log ERROR with `silent_secs`/`threshold_secs` fields
  (`connection.rs`), so a post-change fire rate drop is measurable in
  `errors.jsonl` / CloudWatch; no new metric needed (no new failure mode —
  this removes a false-positive path).

## Plan Items

- [x] Add `WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS = 15` + doc
  - Files: crates/core/src/websocket/activity_watchdog.rs
  - Tests: daily_universe_threshold_is_15_above_dhan_ping_cadence
- [x] Point DailyUniverse clamp arm at the new constant + audit comment
  - Files: crates/app/src/main.rs
  - Tests: (compile — cargo check -p tickvault-app)
- [x] Fix stale ping-cadence comment
  - Files: crates/core/src/websocket/connection.rs
  - Tests: (comment-only)
- [x] Rule-file dated section (pending-approval marked)
  - Files: .claude/rules/project/live-market-feed-subscription.md
  - Tests: (docs)
- [x] ON OPERATOR APPROVAL: filled all pending markers with the dated quote
  `Operator approved 2026-07-02: "approve 15s"`, flipped Status → APPROVED,
  pushed branch + opened PR
  - Files: this plan, activity_watchdog.rs, main.rs, connection.rs,
    live-market-feed-subscription.md
  - Tests: (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy socket, 12s data lull, pings every 10s | No watchdog fire (was: 4 fires at 3s) |
| 2 | Socket silent 15s incl. pings | Watchdog fires, reconnect with SubscribeRxGuard |
| 3 | Indices4Only scope boot | Clamp = 3s, unchanged |
| 4 | Constant regressed to 8 | Ratchet test fails the build |
