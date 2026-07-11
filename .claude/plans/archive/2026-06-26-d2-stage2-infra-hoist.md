# Implementation Plan: D2 Stage 2 — genuine shared-infra hoist above the Dhan branch

**Status:** APPROVED
**Date:** 2026-06-26
**Approved by:** operator (coordinator-authorized "do everything"; held as DRAFT PR for 3-agent review + live boot-deploy crash-monitor before merge)
**Branch:** `claude/d2-stage2-infra-hoist`
**Crate:** `crates/app` (`tickvault-app`)
**Authority:** `active-plan-dhan-cold-start-d2.md` §1.0 (the genuine hoist) >
`active-plan-d2-genuine-infra-hoist.md` (Stage 1 + the verified scatter map) >
`design-first-wall.md` (these 6 sections) > `audit-findings-2026-04-17.md`
Rule 14 (no skeleton / no semantics-changing half-refactor) + Rule 16
(`Arc<Notify>` shutdown) > `websocket-connection-scope-lock.md` (2-WS lock,
`dhan_disable_allowed` gate untouched).

> **This is a BEHAVIOUR-CHANGING reorder of the live trading boot.** Held as a
> DRAFT PR for 3-agent adversarial review + a live boot-deploy (weekend, market
> closed; deploy pipeline has a 60s crash-monitor + auto-rollback) before merge.
> Correctness is paramount.

---

## Design

### Problem (verified against latest main `be49d41b`)
Stage 1 (#1208) split `run_shutdown_fast` into `run_process_runloop(Option<DhanLaneRunHandles>, ...)`
+ `teardown_dhan_lane_tasks` — the run-loop now accepts an OPTIONAL Dhan lane.
But the slow Dhan-ON arm (`main.rs` ~2059→5177) still INTERLEAVES the
PROCESS-shared infra (notifier, health, seal-writer, the two broadcasts, the 3
subscriber tasks, API server) line-by-line with the Dhan LANE work
(IP→auth→universe→WS-pool→tick-processor→order-update→renewal). And the Dhan-OFF
path early-returns at `main.rs:1191` into a DUPLICATE `run_shared_infra_only`
that re-builds the same shared infra a second time. So there are TWO shared-infra
construction sites that can drift.

### The hoist (this PR)
Build the PROCESS-shared infra ONCE, in a single prefix shared by BOTH the
Dhan-OFF and Dhan-ON-slow paths, with the Dhan LANE as a conditional middle, then
the single `run_process_runloop`. New control flow:

```
[shared prefix: config, obs, otel, WAL replay buffers, calendar, feed_runtime,
                feed_health, tick_storage, prev_day_cache, ws_frame_spill]

if dhan_enabled && market_hours && fast_cache {            ◀ FAST ARM (Dhan-ON only)
    ... fast crash-recovery spine ... return run_shutdown_fast(...)   (LEFT BYTE-IDENTICAL)
}

──────── HOIST: PROCESS-shared infra (BOTH Dhan-OFF and Dhan-ON-slow) ────────
notifier (+ Docker auto-start, gated) ; health_status ; seal-writer (+ global_seal)
tick_broadcast_sender ; order_update_sender
slow-boot observability subscriber ; 21-TF aggregator (Engine B) ; tick-storage subscriber
API bearer token (SSM) ; API server (axum::serve, incl. /api/feeds)
──────────────────────────────────────────────────────────────────────────────

if config.feeds.dhan_enabled {                            ◀ DHAN LANE (slow arm only)
    IP verify → auth → instance-lock → QuestDB DDL → universe → tick_writer →
    WS-pool create → WAL reinject → slow-heartbeat → prev-day/prev_oi →
    tick processor (publishes into the hoisted broadcast) → shutdown_notify →
    WS spawn → pool watchdog → schedulers → order-update WS → trading pipeline →
    renewal/sweep/mid-session-watchdog → boot-deadline → periodic health check
    → produce Some(DhanLaneRunHandles)
}                                                          else → None

run_process_runloop(lane_handles_or_none, Some(api_handle), otel, &notifier, &config, calendar)
```

`run_shared_infra_only` is DELETED — the Dhan-OFF path no longer early-returns; it
falls through the shared prefix, skips the fast arm (dhan disabled), skips the
`if config.feeds.dhan_enabled` lane, and hits the SAME `run_process_runloop` with
`lane = None`.

### Load-bearing ordering — preserved BY CONSTRUCTION (per-block proof)

The slow arm's CURRENT relative order already has every hoisted PROCESS block
depending ONLY on PROCESS-prefix values (config, obs, otel, calendar,
feed_runtime, feed_health, tick_storage, prev_day_cache) — NEVER on a lane-built
value (`token_manager` @2158, `subscription_plan` @2931, `token_handle` @2999,
`ws_client_id` @3010, `tick_enricher` @3403, `ws_handles`/`ws_pool_arc` @3654 are
all defined INSIDE the lane, all AFTER the notifier @2076). Proof per block:

| # | Block (current line) | Depends only on | Hoist-safe? |
|---|---|---|---|
| 1 | notifier + Docker (2078) | `config.notification`, `config.features`, `config.infrastructure`, `config.questdb` | yes — already first in slow arm |
| 2 | health_status (2786) | nothing (just `SystemHealthStatus::new()`) | yes |
| 3 | seal-writer + global_seal (2660-2717) | `config.questdb` | yes |
| 4 | tick_broadcast_sender (3130) | `TICK_BROADCAST_CAPACITY` const | yes |
| 5 | order_update_sender (4630) | `ORDER_UPDATE_BROADCAST_CAPACITY` const | yes |
| 6 | obs subscriber (3150-3157) | `tick_broadcast_sender` (#4), `config.questdb` | yes |
| 7 | aggregator Engine B (3249-3253) | `tick_broadcast_sender` (#4), `prev_day_cache`, `trading_calendar` | yes |
| 8 | tick-storage subscriber (3258-3272) | `tick_broadcast_sender` (#4), `tick_storage`, `config.in_mem` | yes |
| 9 | API server (4799-4854) | `config`, `health_status` (#2, via Arc::clone), `feed_runtime`, `feed_health` | yes |

**Subscribe-before-publish (the zero-tick-loss invariant):** the obs/aggregator/
tick-storage `.subscribe()` calls (#6/#7/#8) run in the hoisted PREFIX. The ONLY
publisher into `tick_broadcast_sender` is the lane's `run_tick_processor` (3582),
which runs LATER inside the `if config.feeds.dhan_enabled` lane block. So every
subscriber is registered before the processor publishes — preserved by
construction. (Dhan-OFF: nothing publishes; the subscribers idle on the empty
broadcast — identical to today's `run_shared_infra_only`.)

**WS-pool create vs broadcast:** today WS-pool create (3043, lane) runs BEFORE
`tick_broadcast_sender` (3130). `create_websocket_pool`'s args do NOT include
`tick_broadcast_sender` (verified), so moving the broadcast to the prefix (before
the lane) is safe — the lane processor still gets `tick_broadcast_sender.clone()`
from the hoisted value.

**WS-pool create BEFORE tick processor BEFORE WS spawn:** all three stay in the
lane block in their current relative order; the hoist does not move them.

**run-loop AFTER both shared infra and (on ON-boot) the lane:** the
`run_process_runloop` call is the last statement, same as today.

**seal-writer `global_seal_sender` is a process singleton (first-wins):** the
single hoisted call installs it once; the fast arm's own seal-writer never
co-runs in one boot (the fast arm returns before the hoist).

**API binds exactly once:** only the hoisted prefix binds the port; the slow
arm's old API bind is removed. The fast arm's API bind is untouched (separate
boot that returns first).

**Tick-path inits stay in the LANE:** lines 2722-2784 (prev_close cache,
first_seen set, tick_spill_drain, disk_health_watcher) are Dhan-tick-path inits
that `run_shared_infra_only` does NOT run today — they stay inside the lane block
(after the prefix), preserving Dhan-OFF isolation.

### Scope boundary (honest)
- The FAST arm (Dhan-ON crash-recovery, ~1222→2057) is LEFT byte-identical (its
  own broadcast/aggregator/seal-writer/API/run-loop), per design L10/D2d. Reached
  ONLY at boot with a valid cache in market hours; never from a runtime toggle.
- This PR does NOT add the runtime cold-start (D2b), the `LaneState` FSM, or
  `start_dhan_lane`. It ONLY hoists shared infra + deletes `run_shared_infra_only`
  so BOTH paths share one construction. No new pub surface without a call site
  (Rule 14).

---

## Edge Cases

| # | Edge case | Behaviour after hoist |
|---|---|---|
| 1 | Dhan-ON slow boot (prod default) | Shared infra built once in prefix; lane built in `if dhan_enabled`; run-loop runs. Same tasks, reordered construction. |
| 2 | Dhan-OFF / Groww-only boot | Prefix builds API (+ /api/feeds) + seal-writer + aggregator; lane skipped; run-loop runs with `lane=None`. Replaces `run_shared_infra_only`. |
| 3 | Dhan-ON FAST boot (cache + market hours) | FAST arm matches FIRST and returns before the hoist — byte-identical, untouched. |
| 4 | Dhan-OFF, no Groww (no feed) | Same as edge 2; warn logged; shared infra + run-loop keep the process alive. |
| 5 | IP verify early-`return` (live, IP mismatch) | Stays inside the lane block (Dhan-ON only). On Dhan-OFF the IP gate never runs. |
| 6 | QuestDB down at boot | seal-writer/API construct lazily (ILP lazy connect); aggregator idles. Lane's QuestDB DDL surfaces BOOT-01/02. Same as today. |
| 7 | Docker auto-start disabled | Hoisted notifier/Docker honours `infrastructure.auto_start_docker` exactly as today. |

---

## Failure Modes

| Failure | Result | Preserved-from-today? |
|---|---|---|
| Notifier strict-init fails | `Err` → REFUSE BOOT (systemd restarts). Same `initialize_strict`+coalescer logic. | yes |
| API bearer token SSM fetch fails | `?` → boot fails (hard-fail, no env fallback). Same. | yes |
| API bind fails (port in use) | `?` → boot fails; now in the prefix instead of the slow arm; same outcome. | yes (moved) |
| Dhan auth fails | inside the lane; same retry/halt as today. | yes |
| seal-writer construct fails | `error!` + continue. Same. | yes |
| Run-loop teardown | `run_process_runloop` → `teardown_dhan_lane_tasks` (lane) — unchanged from Stage 1. | yes |

**Governing rule:** the hoist NEVER changes WHICH tasks run or their relative
publish/subscribe order — only the lexical construction site moves. Any ordering
that cannot be preserved → STOP + report (Rule 14).

---

## Test Plan

Scoped to `crates/app`.
- `cargo build -p tickvault-app` clean (0 warnings).
- `cargo test -p tickvault-app` green — esp. `feed_toggle_lifecycle_guard`,
  `per_feed_boot_isolation_guard`, `run_loop_split_guard`, `shutdown_sequence_guard`.
- `cargo fmt -p tickvault-app --check`.
- banned-pattern scanner + pub-fn-test + pub-fn-wiring guards exit 0.
- NEW guard test `crates/app/tests/d2_stage2_hoist_guard.rs`:
  - `run_shared_infra_only_is_deleted` — source-scan: the fn no longer exists.
  - `single_shared_infra_construction` — exactly ONE `axum::serve` +
    ONE `spawn_engine_b_aggregator` call reachable in `main()` outside the fast arm.
  - `dhan_lane_is_wrapped_in_feed_gate` — a `if config.feeds.dhan_enabled {`
    wrapper exists in `main()` after the fast arm.
  - `aggregator_subscribe_precedes_dhan_tick_publish` — the hoisted
    `spawn_engine_b_aggregator` / obs `.subscribe()` site precedes the lane
    `run_tick_processor` publish site in `main()`.
- `dhat` N/A (cold control-plane boot, not a hot path).
- Commit `.claude/hooks/.test-count-baseline` bump for the net test rise.

---

## Rollback

Single-PR, single-file (`crates/app/src/main.rs`) + test/guard additions.
Rollback = `git revert` the squash-merge commit. Held as DRAFT + boot-tested
(deploy crash-monitor + auto-rollback) before merge. The FAST arm is
byte-identical. No schema, no DEDUP-key, no Cargo.toml, no config changes — pure
control-flow refactor of one binary's boot.

---

## Observability

No new metrics/counters/audit tables (a construction-site move). The hoisted
prefix preserves every existing log line / `NotificationEvent` / Prometheus gauge
(`StartupComplete`, boot-timing Telegram, `tv_lifecycle_enricher_attached`,
`/health`, `/api/feeds`). The new guard tests are the mechanical ratchet that the
hoist stays single-construction + ordering-correct.

---

## Per-Item Guarantee Matrix (cross-ref)

Carries the 15-row + 7-row matrices of
`.claude/rules/project/per-wave-guarantee-matrix.md` by reference. Control-flow
refactor: zero new tick-drop path (publish-subscribe order preserved by
construction — proven in Design); no new hot-path allocation; the 2-WS Dhan lock +
`dhan_disable_allowed` gate untouched; no new WebSocket endpoint; composite-key/
DEDUP unchanged (no table edits). Honest envelope: behaviour-CHANGING reorder,
boot-test-gated, fast arm byte-identical, slow + OFF paths share one construction.
