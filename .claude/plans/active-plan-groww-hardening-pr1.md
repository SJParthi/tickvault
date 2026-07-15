# Implementation Plan: Groww stall-watchdog relaunch handshake grace (hardening PR-1)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) via coordinator session, 2026-07-14 — lossless-reconnect build PR-1 authorized; grace=15s default approved

> Guarantee matrices: this plan cross-references the mandatory 15-row + 7-row
> guarantee matrix in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (applies in full to this item; cold supervision path — no hot-path rows are
> weakened; no new tick-drop path is introduced).

## Plan Items

- [x] Rule-file semantics update (2026-07-14 relaunch handshake grace)
  - Files: .claude/rules/project/feed-stall-watchdog-error-codes.md
  - Tests: n/a (docs; cross-ref test already green — no new ErrorCode)

- [x] Grace the stall kill across sidecar relaunches
  - Files: crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_replay_2026_07_14_incident_child_in_grace_not_killed,
    test_stall_kill_fires_past_grace, test_grace_boundary_is_strict,
    test_handshake_grace_const_pinned

## Design

The 2026-07-14 14:59 IST incident: a server-side NATS `unexpected EOF` left the
feed silent; the stall watchdog killed+relaunched the sidecar, then re-killed
each freshly-relaunched child 3 more times (stall_secs 31/32/33/34) because the
feed-level last-tick age (`FeedHealthRegistry::last_tick_age_secs`) never
resets on relaunch. Fix: `crates/app/src/groww_sidecar_supervisor.rs` adds
`const FEED_STALL_HANDSHAKE_GRACE_SECS: u64 = 15` next to
`FEED_STALL_RESTART_SECS` (30, unchanged). The pure decision fn
`should_restart_on_stall` gains a `child_age_secs: u64` parameter; a kill now
requires the existing conditions AND `child_age_secs >
FEED_STALL_HANDSHAKE_GRACE_SECS` (strict `>`). `supervise_child` stamps
`child_spawned_at = Instant::now()` at each (re)launch and passes
`child_spawned_at.elapsed().as_secs()` at the stall call site. Nothing else
changes: the never-streamed 300s arm, the `StallRestartStorm` bound, Telegram
wording, cooldowns, and the disable-toggle re-read are byte-identical.

## Edge Cases

- Child age exactly 15s → NOT killed (strict `>`); age 16s with feed silent → killed.
- Relaunch slower than 15s to first tick (genuinely dead relaunch) → still
  killed at a bounded ~16-17s cadence; storm escalation (>5 in 300s) arrives
  ≤ ~2 min instead of ~36s — stated, accepted trade.
- A subscribed-but-silent LIVE socket (child age » 15s) is unaffected — the
  grace spans only the first 15s after a spawn; the classic 30s arm owns
  liveness past it.
- Pre-open / market-closed / no-known-last-tick arms unchanged (the grace is
  ANDed onto the existing predicate, never widening it).
- 2026-07-14 replay: kill #1 identical (~31s detection); kills #2-#4
  structurally impossible (child ages 1-3s « 15s grace).

## Failure Modes

- Grace too long would delay recovery of a dead relaunch → bounded at 15s
  (operator-approved default), const-pinned by a ratchet test (5 < grace < 30).
- A regression removing the child-age condition re-opens the restart storm →
  the replay test (feed age 31, child age 5 → false) fails the build.
- Instant::elapsed monotonic clock — no wall-clock skew exposure (BOOT-03
  class not applicable).
- If the supervisor task dies, FEED-SUPERVISOR-01 respawn semantics are
  untouched (no change to that arm).

## Test Plan

- Update every existing `should_restart_on_stall` test call with a large
  child age (999) so prior semantics are preserved verbatim.
- New: `test_replay_2026_07_14_incident_child_in_grace_not_killed` (feed age
  31, child age 5 → false), `test_stall_kill_fires_past_grace` (child age 16 →
  true), `test_grace_boundary_is_strict` (child age 15 → false),
  `test_handshake_grace_const_pinned` (== 15 and 5 < grace < 30).
- `cargo test -p tickvault-app` green; `cargo fmt --all -- --check` clean.
- Scope: crates/app only per testing-scope.md (no crates/common change).

## Rollback

Single-commit revert of the code change restores pre-grace semantics exactly
(the added parameter and const are confined to
`crates/app/src/groww_sidecar_supervisor.rs`); the rule-file section is then
superseded by a dated note per house convention. No schema, no config, no
persisted-state migration — rollback is a pure `git revert`.

## Observability

No new ErrorCode, counter, or Telegram wording. Existing signals fully
retained: `tv_feed_sidecar_stall_restart_total{feed}` (per kill, warn!-level
per-restart / error! storm arm), the `tv-<env>-feed-stall-restarts` ≥3-per-15m
pager, the `tv-<env>-errcode-feed-stall-01` storm tripwire, the
`stall_restarted` ws_event_audit rows (`dual-feed-scoreboard-error-codes.md`
§2), and FEED-REJECT-01 signatures. Behavioural delta is visible as a lower
kill cadence during a relaunch handshake — measured by the same counters.
