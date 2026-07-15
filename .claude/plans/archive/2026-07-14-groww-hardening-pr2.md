# Implementation Plan: Groww Sidecar Escalation-Exit + Reconnect Heartbeat (Hardening PR-2)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) via coordinator session, 2026-07-14 — lossless-reconnect build PR-2 authorized

> Guarantee matrices: the 15-row + 7-row per-item guarantee matrices apply by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (this plan's items are cold-path supervision/observability only — no
> hot-path, no new table, no new WebSocket).

## Design

2026-07-14 incident: a Groww NATS `unexpected EOF` left the sidecar's own
in-process reconnect (5s force-close + ladder) unrecovered for >30s, so the
Rust stall watchdog killed it — WHY the in-process reconnect failed is
unknown because no per-cycle outcome was captured. Design S2 (coordinator
approved): the sidecar (`scripts/groww-sidecar/groww_sidecar.py`)
self-escalates FASTER than the watchdog, pager-safely:

1. **Per-cycle reconnect heartbeat** — after each force-close/reconnect
   cycle, print ONE fixed-shape stderr line
   `groww sidecar reconnect-cycle: attempt=N outcome=<slug>` where the slug
   is derived from the existing exception/phase classification
   (`connected` / `failed_auth` / `failed_connect` / `failed_subscribe` /
   `failed_other`).
2. **Escalation-exit** — after 2 CONSECUTIVE fully-failed cycles, print the
   FIXED marker `GROWW SIDECAR ESCALATION-EXIT: repeated reconnect failure —
   exiting for clean relaunch` and `sys.exit(1)` so the Rust supervisor
   (`crates/app/src/groww_sidecar_supervisor.rs`) relaunches the sidecar cold
   (fresh SSM token read + full re-subscribe). Counter resets on any
   successful reconnect.
3. **Rust drain mapping** — the pipe drain classifies the escalation-exit
   marker (prefix-anchored) as a new `SidecarLineClass` variant and routes it
   through the SAME `tv_feed_sidecar_stall_restart_total{feed}` counter +
   `StallRestartStorm` record a watchdog kill uses — pager parity, no new
   counter. Heartbeat lines classify Info (tracing only).
4. **S3 capture-only warm-up** — after a successful re-subscribe, a fail-soft
   SDK-cache last-price snapshot appended to the NDJSON capture ONLY (never a
   liveness source). Skipped if no safe cache accessor exists (fail-soft is
   the design).

## Edge Cases

- Marker appearing MID-line (log noise embedding) must NOT classify — the
  Rust match is prefix-anchored, unit-tested.
- A success between two failures resets the consecutive counter — no exit on
  interleaved fail/ok/fail.
- The .py and .rs marker literals must stay lockstep — a source-scan test in
  the Rust file greps both files for the literal.
- Off-hours reconnect cycles print heartbeats but the escalation-exit is a
  process exit either way; the supervisor's off-hours relaunch/backoff
  semantics are unchanged (no watchdog threshold change).
- Heartbeat lines must not match any existing auth/entitlement latch
  classification (they carry no `error [` prefix and no reject keywords).

## Failure Modes

- **Escalation-exit loop** (provider-side persistent reject): each self-exit
  increments the stall counter, so the ≥3-per-15-min
  `tv-<env>-feed-stall-restarts` pager and the FEED-STALL-01 storm escalation
  BOTH keep firing — self-heal can never silence the alarms (the pager-safety
  core of S2).
- **sys.exit(1) mid-write**: the NDJSON writer appends line-atomically and
  the bridge tails by byte offset draining only complete newline-terminated
  lines — a partial trailing line is re-read after relaunch, never lost.
- **Warm-up accessor raising**: wrapped try/except, fail-soft — capture
  continues without the snapshot.
- **Classifier drift**: the lockstep source-scan test fails the build if the
  .py marker literal and the .rs matcher literal diverge.

## Test Plan

- `python3 -m py_compile scripts/groww-sidecar/groww_sidecar.py` (syntax).
- New Rust unit tests in `crates/app/src/groww_sidecar_supervisor.rs`
  (`mod escalation_exit_tests`): marker line classifies to the new class;
  heartbeat line classifies Info; mid-line marker does NOT classify
  (prefix-anchor); lockstep literal source-scan across the .py and .rs files.
- `cargo test -p tickvault-app` full crate suite green.
- `cargo fmt --all` clean.

## Rollback

Revert the branch commits — the sidecar returns to in-process-ladder-only
reconnects (the 30s watchdog kill remains the backstop, unchanged by this
PR), and the drain classifier loses only the new variant. No table, no
config, no alarm change to unwind. The rule-file subsection is dated and can
be marked superseded per house convention.

## Observability

- Heartbeat line per reconnect cycle (stderr → supervisor drain → tracing)
  answers WHY a reconnect failed next incident.
- Escalation-exit marker → `tv_feed_sidecar_stall_restart_total{feed}` +
  `StallRestartStorm` (existing pre-registered counter; both existing pagers
  preserved — no new metric, no alarm change).
- Rule-file runbook: `.claude/rules/project/feed-stall-watchdog-error-codes.md`
  "2026-07-14 Update" subsection (committed in this PR).
- Honest envelope: targets ~20-25s worst-case gap; NO number claimed until
  live-measured.

## Plan Items

- [x] Rule-file subsection (S2 + S3 semantics)
  - Files: .claude/rules/project/feed-stall-watchdog-error-codes.md
- [ ] Sidecar heartbeat + escalation-exit (+ warm-up if safe accessor exists)
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: py_compile
- [ ] Rust drain mapping + tests
  - Files: crates/app/src/groww_sidecar_supervisor.rs
  - Tests: escalation_exit_tests (classify marker / heartbeat Info / prefix-anchor / lockstep literal)
