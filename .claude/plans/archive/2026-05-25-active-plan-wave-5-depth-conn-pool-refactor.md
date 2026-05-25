# Wave 5 — Depth Conn-Pool Receiver-Side Refactor

**Status:** APPROVED (operator: "yes first merge the current pr and pull
the latest changes from main and then start this new dude" — 2026-05-01)
**Branch:** `claude/wave-5-depth-conn-pool-refactor` (forked from main
`b507675` after PR #418 merge)
**Date:** 2026-05-01 (Fri night) — validate over Sat/Sun before Mon market open
**Prior PR:** #418 merged — orchestrator (Sender side) shipped; this PR
ships the conn-pool (Receiver side) so dynamic Swap20/Swap200 actually
take effect.

## Operator spec (verbatim 2026-05-01)

> "for depth 20 i clearly told you nifty atm ce plus or minus 24 and
>  nifty pe plus or minus 24 meanwhile same for banknifty and for the
>  last one connection alone top 50 volume sorted by percentage desc
>  where sensex should be excluded bro meanwhile for depth 200 also
>  same which is top 5 volume sorted by percentage desc excluded sensex"

> "see even this one also can be found only at 9.12 close right dude
>  because only then we will get to know the real atm right bro for
>  both depth 20 and depth 200 and for every one minute it should
>  again check right to find the atm and even for resubscribe lets
>  say from atm it should check whether it moved till these only then
>  resubscription should happen right dude"

## Target subscription topology (Option B locked)

| Conn | Type | Owner | Subscription |
|---|---|---|---|
| 1 | depth-20 | static | NIFTY ATM ± 24 CE (49 SIDs) |
| 2 | depth-20 | static | NIFTY ATM ± 24 PE (49 SIDs) |
| 3 | depth-20 | static | BANKNIFTY ATM ± 24 CE (49 SIDs) |
| 4 | depth-20 | static | BANKNIFTY ATM ± 24 PE (49 SIDs) |
| 5 | depth-20 | dynamic | top-50 NSE_FNO contracts by `change_pct DESC`, SENSEX excluded |
| 6 | depth-200 | dynamic | top-5 SENSEX-excluded contract slot 0 |
| 7 | depth-200 | dynamic | top-5 SENSEX-excluded contract slot 1 |
| 8 | depth-200 | dynamic | top-5 SENSEX-excluded contract slot 2 |
| 9 | depth-200 | dynamic | top-5 SENSEX-excluded contract slot 3 |
| 10 | depth-200 | dynamic | top-5 SENSEX-excluded contract slot 4 |

ATM determined at 09:13:00 IST from finalized 09:12 close. Per-minute
re-check; resubscribe only on drift ≥ 3 strikes (existing
`DEPTH_ATM_REBALANCE_THRESHOLD`).

## What ships in PR #418 (already merged)

- `select_single_side_contracts(...)` helper (additive primitive)
- `spawn_depth_20_dynamic_conn5_task` orchestrator (Sender side)
- `spawn_depth_200_dynamic_pool_task` orchestrator (Sender side)
- `[features] depth_dynamic_top_volume = true` (default ON)
- `Depth20Dyn03TopGainersEmpty`, `Depth200Dyn01TopGainersEmpty` ErrorCodes
- 25 ratchet tests on the orchestrators

## What this PR ships (Receiver side)

| # | Commit | Files | LoC | Risk |
|---|---|---|---|---|
| 1 | depth-20 mixed→single-side iteration | `crates/app/src/main.rs` (lines 3140-3630) | ~150 | High |
| 2 | spawn 5th depth-20 dynamic conn consuming `d20_conn5_cmd_rx` | `crates/app/src/main.rs` + `depth_connection.rs` deferred-mode handling | ~80 | Medium |
| 3 | depth-200 5-slot dynamic replacement (replaces 4 ATM-fixed) | `crates/app/src/main.rs` (lines 3633-3950) | ~200 | High |
| 4 | depth_rebalancer keys: `NIFTY` → `NIFTY-CE`/`NIFTY-PE` (4 keys instead of 2) | `crates/core/src/instrument/depth_rebalancer.rs` + tests | ~50 | Medium |
| 5 | phase2_scheduler InitialSubscribe20 fan-out: 4 single-side keys | `crates/core/src/instrument/phase2_scheduler.rs` + tests | ~60 | Medium |
| **Total** | | | **~540** | |

## Per-commit gates

Each commit MUST land green on:

1. `cargo check -p tickvault-core -p tickvault-app`
2. `cargo test -p <touched_crate> --lib`
3. `bash .claude/hooks/banned-pattern-scanner.sh`
4. `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all`
5. `bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"`

After commit 5: `FULL_QA=1 make scoped-check` workspace-wide.

## Per-Item Guarantee Matrix (mandatory per `per-wave-guarantee-matrix.md`)

| Demand | Mechanical proof artefact |
|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` 100% min — coverage-gate.sh |
| 100% audit coverage | New `subscription_audit_log` rows for every (underlying, side) Swap20/Swap200 — already wired |
| 100% testing coverage | 22 categories per `testing.md` for `tickvault-app` + `tickvault-core` |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates |
| 100% performance | DHAT zero-alloc on hot path (boot is cold path; ratchet stays at existing budget) |
| 100% monitoring | 7-layer telemetry already in place via PR #418 — this PR consumes existing counters |
| 100% logging | tracing macros only; ERROR → Telegram |
| 100% alerting | DEPTH-DYN-02 alert pre-shipped in PR #418 |
| 100% security | banned-pattern + secret-scan + Secret<T> + security-reviewer agent |
| 100% security hardening | `unused_must_use` lint already enabled |
| 100% bugs fixing | adversarial 3-agent review BEFORE commit + AFTER commit |
| 100% scenarios covering | `disaster-recovery.md` Scenarios 5/6/8/14/15 cover all WS reconnect paths |
| 100% functionalities covering | every new pub fn has call site + test |
| 100% code review | adversarial 3-agent on diff before AND after impl |
| 100% extreme check | ratchet tests fail build on regression |

| Resilience demand | Honest envelope |
|---|---|
| Zero ticks lost | Bounded zero loss — depth ticks flow through existing `DeepDepthWriter` 3-tier rescue→spill→DLQ |
| WS never disconnects | DETECT ≤5s via `respawn_dead_connections_loop`; reconnect with `SubscribeRxGuard` (PR #337); sleep-until-open post-close |
| Never slow/locked/hanged | Boot-path code; not on hot path |
| QuestDB never fails | ABSORB via 3-tier; this PR adds zero new persist sites |
| O(1) latency | Boot-time only |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 |
| Real-time proof | 7-layer telemetry already in place |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage:
≤60s QuestDB outage absorbed by rescue→spill→DLQ; ≤600K rescue ring
capacity; bench-gated O(1) hot path; composite-key uniqueness;
chaos-tested 70h sleep/wake. Beyond the envelope, DLQ NDJSON catches
every payload as recoverable text.

## Validation plan over weekend

1. **Sat morning** — local `make scoped-check` workspace-wide; 0 failures
2. **Sat afternoon** — local boot test: `make run` against local Docker
   stack; verify boot logs show 5 depth-20 + 5 depth-200 spawns
3. **Sun** — buffer for any Saturday issues
4. **Mon 09:00 IST** — pre-market dispatch of operator's tracking SELECT
   from `docs/operator/track-2-monotonicity-select.md`
5. **Mon 09:13:30 IST** — operator confirms via Telegram:
   - `MarketOpenStreamingConfirmation` shows depth_20=5, depth_200=5
   - `MarketOpenDepthAnchor` fires for NIFTY + BANKNIFTY
   - `Phase2Complete` includes new single-side underlying counts
6. **Mon 09:15-16:00** — operator monitors `tv_depth_20_connections`
   and `tv_depth_200_connections` gauges = 5 each, sustained

## Operator pending (independent of this PR)

- Mon May 4 09:45 IST Track 2 monotonicity SELECT (already in
  `docs/operator/track-2-monotonicity-select.md`)
- Gmail-send Item 26 L3 Dhan support email per PR #414

## Files NOT changed by this PR

- `crates/storage/src/movers_unified_*.rs` — already shipped in PR #418
- `crates/core/src/pipeline/volume_monotonicity_guard.rs` — already wired
- `crates/app/src/bhavcopy_pipeline.rs` — already running

This PR is strictly receiver-side wiring for the Sender side that PR #418
shipped.
