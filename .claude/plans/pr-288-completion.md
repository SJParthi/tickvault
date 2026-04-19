# Implementation Plan: PR #288 — #5/#6/#7 completion

**Status:** APPROVED
**Date:** 2026-04-19
**Approved by:** Parthiban ("yes go ahead with everything dude")
**Branch:** claude/review-automation-coverage-LlHex
**PR:** #288

## Context

PR #288 originally shipped 10 candidate items. 7 are shipped. The three
remaining items from `.claude/plans/100pct-audit-tracker.md` (Phase 10.2,
10.3, 11.x) are multi-day chaos/observation work. This plan ships the
tractable part of #5 now and defers #6/#7 with a documented design so a
future session can pick them up without rediscovering the context.

## Plan Items

### #5 — Depth sequence-hole detector (SHIPPING THIS SESSION)

- [x] Build `crates/core/src/pipeline/depth_sequence_tracker.rs` with 8 unit tests.
- [x] Wire to 20-level depth path (optional — tracker is pluggable).
- [x] Guard test that the tracker is segment-aware (I-P1-11 compliance).

### #6 — Zero-tick-loss chaos test (DEFERRED — plan only)

DESIGN: `crates/core/tests/chaos_zero_tick_loss.rs` should:
1. Spin up an in-process fake WS server (`tokio-tungstenite` server bind).
2. Connect a real tickvault `WebSocketConnection` to it.
3. Stream 1000 synthetic Full packets, pause 60s, resume.
4. Assert: reconnect within `WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS + 5s`;
   zero duplicate ticks after DEDUP; zero out-of-order.

**Blockers (why deferred):**
- No in-process WS server test harness today. Building one is ~400 lines
  of plumbing. Belongs in its own PR with architect sign-off on the
  fake-server abstraction (will it stay in-tree? Be used by more tests?).

### #7 — Phase 11 chaos integration (DEFERRED — plan only)

DESIGN: CI-level chaos tests that kill/restart Docker services:
1. `crates/storage/tests/chaos_questdb_kill.rs`: spill queue grows, app
   stays up, tick-count matches after restart.
2. `crates/storage/tests/chaos_valkey_kill.rs`: token cache falls back
   to SSM, no crash.
3. `crates/core/tests/chaos_ws_force_disconnect.rs`: WS reconnect <2s,
   zero tick loss across the gap.

**Blockers (why deferred):**
- CI runner must have Docker-in-Docker or a dedicated VM.
- GitHub Actions free tier supports docker service containers, but
  `docker kill` inside that service is opaque from the workflow.
- Most reliable path: self-hosted runner. Cost-budget-aware per
  `.claude/rules/project/aws-budget.md` — runs into AWS spend.
- Operator decision required before shipping: (a) paid CI lane,
  (b) self-hosted runner, (c) nightly-only on the AWS EC2 instance.

## Scenarios covered by #5

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Fresh start, first packet | Sequence recorded, no hole |
| 2 | Sequential N, N+1 | No hole |
| 3 | Single skip N, N+2 | Hole count +1 |
| 4 | Big gap N, N+1000 | Hole count +1 (events, not missed packets) |
| 5 | Rollback (N, then 1) | Reset; no hole counted |
| 6 | Duplicate (N, N) | Duplicate +1, no hole |
| 7 | Bid advances, ask stays | Independent tracking |
| 8 | Same id on IDX_I vs NSE_FNO | Segment-aware (I-P1-11) |

## Out of scope

- 200-level depth: bytes 8-11 are row-count, not sequence.
- Main feed: no sequence in 8-byte header. LTT gaps handled by RISK-GAP-03.
- Alert thresholds: operator decides via Grafana rules on the metric.
