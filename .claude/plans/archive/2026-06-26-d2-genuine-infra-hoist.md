# Implementation Plan: D2 genuine shared-infra hoist (prerequisite for start_dhan_lane)

**Status:** IN_PROGRESS

> **HONEST PROGRESS (2026-06-26):** Stage 1 (the `run_shutdown_fast` →
> `run_process_runloop` + `teardown_dhan_lane_tasks` split, design C3) is
> COMPLETE, compiled clean, and behaviour-identical (verified — all boot-isolation
> + lifecycle + shutdown guards green). Stage 2 (the actual scatter-relocation of
> notifier/health/seal-writer/broadcast/aggregator/API ABOVE the Dhan lane to
> eliminate the duplicate `run_shared_infra_only`) is NOT done — see §"Stage 2
> blocker" below. This PR ships Stage 1 only, as a verified prerequisite.
**Date:** 2026-06-26
**Approved by:** operator (coordinator-authorized "do everything"; held as DRAFT for boot-test)
**Branch:** `claude/d2-genuine-infra-hoist`
**Crate:** `crates/app` (`tickvault-app`)
**Authority:** `active-plan-dhan-cold-start-d2.md` §1.0 (the D2-pre genuine hoist) >
`d2a-blocker-analysis.md` (why #1207's duplicate `run_shared_infra_only` did NOT
isolate the Dhan-ON spine) > `design-first-wall.md` (these 6 sections) >
`audit-findings-2026-04-17.md` Rule 14 (no skeleton / no semantics-changing
half-refactor) + Rule 16 (`Arc<Notify>` shutdown).

> **This is a BEHAVIOUR-CHANGING reorder of the live trading boot** — NOT
> byte-identical. It is held as a DRAFT PR for an operator boot-test before
> merge (per the coordinator). Correctness is paramount.

---

## Design

### The problem (verified)
`#1207` (latest `main` @ `3343abdc`) added a **duplicate** `run_shared_infra_only`
fn for the Dhan-OFF path only. The Dhan-ON SLOW arm (`main.rs` ~2059→5177) still
**interleaves** the PROCESS-shared infra (`tick_broadcast_sender`, the 21-TF
aggregator + its subscribe, the seal-writer, the API server, and the post-boot
run-loop) line-by-line with the Dhan-LANE work (auth → universe → WS pool →
processor → order-update → token-renewal). So the slow arm is NOT the
"now-isolated Dhan spine" D2a's pure extraction needs.

### The hoist (this PR)
Construct the PROCESS-shared infra ONCE, in a single section shared by BOTH the
Dhan-OFF and the Dhan-ON SLOW paths, with the Dhan LANE as the conditional
middle. New control flow in `main()`:

```
[shared prefix: config, obs, WAL replay buffers, calendar, feed_runtime,
                feed_health, tick_storage, prev_day_cache, groww lanes, dhan watcher]

if dhan_enabled && market_hours && fast_cache {            ◀ FAST ARM (Dhan-ON only)
    ... fast crash-recovery spine ... return run_shutdown_fast(...)   (LEFT BYTE-IDENTICAL)
}

──────── HOIST: PROCESS-shared infra (BOTH Dhan-OFF and Dhan-ON-slow) ────────
notifier (+ Docker auto-start, gated) ; health_status ; seal-writer
tick_broadcast_sender ; order_update_sender
slow-boot observability subscriber ; 21-TF aggregator subscribe ; tick-storage subscriber
API bearer token (SSM) ; API server (axum::serve, incl. /api/feeds)
──────────────────────────────────────────────────────────────────────────────

if dhan_enabled {                                          ◀ DHAN LANE (slow arm only)
    IP verify → auth → QuestDB DDL → universe → WS-pool create →
    tick processor (publishes into the hoisted broadcast) → WS spawn →
    pool watchdog → schedulers → order-update WS → token renewal/sweep/profile-watchdog
    → produce DhanLaneHandles
}

run_process_runloop(lane_handles_or_none, shared_infra_handles)   ◀ PROCESS run-loop (BOTH)
```

`run_shared_infra_only` is DELETED — its job folds into the single hoisted
section + the shared run-loop.

### `run_shutdown_fast` split (design C3)
Split into:
- `run_process_runloop(...)` — the PROCESS run-loop: market-close timer,
  partition-detach, `wait_for_shutdown_signal`, then teardown. Runs for BOTH
  Dhan-OFF (no lane handles) and Dhan-ON-slow (lane handles present).
- `teardown_dhan_lane_tasks(...)` — the lane-only teardown (renewal → order-update
  → graceful WS close → processor flush). This is what D2b's `stop_dhan_lane`
  will call at runtime. For THIS PR it is invoked from the run-loop's existing
  teardown path so boot behaviour is preserved; D2b wires the runtime stop.
The FAST arm keeps calling `run_shutdown_fast` (renamed call target preserved or
kept as a thin wrapper) so the fast path stays byte-identical.

### Load-bearing ordering — preserved by construction
1. **aggregator/observability/tick-storage `.subscribe()` BEFORE the Dhan tick
   processor publishes.** The hoisted section runs the three `.subscribe()` calls
   in the PROCESS prefix; the Dhan lane's `run_tick_processor` (the only publisher)
   runs AFTER, inside the `if dhan_enabled` lane block. Subscribe-before-publish
   holds. (Dhan-OFF: nothing publishes; aggregator idles on the empty channel —
   identical to today's `run_shared_infra_only`.)
2. **WS pool create BEFORE tick processor BEFORE WS spawn** — all three stay
   inside the lane block in their current relative order; the hoist does not
   move them.
3. **run-loop AFTER both shared infra and (on ON-boot) the lane** — the run-loop
   is the last statement, same as today.
4. **seal-writer `global_seal_sender` is a process singleton (first-wins)** — the
   hoisted single call installs it once; the fast arm's own `spawn_seal_writer_loop`
   is a harmless idempotent skip if the fast arm runs (but the fast arm returns
   before the hoist, so it never double-runs in one boot).
5. **API server binds once** — only the hoisted section binds the port; the slow
   arm's old API bind is removed. The fast arm's API bind is untouched (separate
   boot that returns first).

### Scope boundary (honest)
- The **FAST arm** (Dhan-ON crash-recovery, `main.rs` ~1222→2057) is LEFT
  byte-identical (its own broadcast/aggregator/seal-writer/API/run-loop), per
  design L10/D2d. It is only ever reached at boot with a valid cache in market
  hours; never from a runtime toggle. Unifying it is the separate D2d follow-up.
- This PR does NOT add the runtime cold-start path (D2b), the `LaneState` FSM, or
  `start_dhan_lane`. It ONLY hoists shared infra + splits the run-loop so D2a/D2b
  become tractable. No new pub surface without a call site (Rule 14).

### Stage 2 blocker (why the full hoist is NOT in this PR — Rule 14 honesty)

A verified map of the slow arm (`crates/app/src/main.rs`) shows the PROCESS-shared
items are scattered across ~2,750 lines, deeply interleaved with the Dhan-LANE
work and with load-bearing ordering between them:

```
2076  notifier + Docker        (PROC)   ◀ shared
2113  IP verify                (LANE)
2152  auth (TokenManager)      (LANE)
2201-2337 instance-lock        (LANE)
2589  universe load            (LANE)
2669  seal-writer (inline)     (PROC)   ◀ shared, AFTER auth/universe
2728-2783 boot-inits           (LANE-ish: prev_close/first_seen/spill/disk — Dhan tick path)
2786  health_status            (PROC)   ◀ shared
2788  tick_writer              (LANE)   ◀ feeds the LANE processor
3044  WS-pool create           (LANE)
3130  tick_broadcast_sender    (PROC)   ◀ shared
3151  obs subscribe            (PROC)   ◀ shared, MUST precede processor publish
3249  aggregator subscribe     (PROC)   ◀ shared, MUST precede processor publish
3259  tick-storage subscribe   (PROC)   ◀ shared
3582  tick PROCESSOR           (LANE)   ◀ the only publisher into the broadcast
3659  WS spawn                 (LANE)
3689-4625 schedulers           (LANE)   ◀ subscribe to broadcast + use health_status
4630  order_update_sender      (PROC)   ◀ shared
4702  order-update WS          (LANE)
4759  trading pipeline         (LANE)
4825  API server               (PROC)   ◀ shared
4859  token renewal/sweep      (LANE)
```

The genuine hoist requires RELOCATING the 7 scattered shared blocks (notifier,
health, seal-writer, broadcast, order_broadcast, the 3 subscriber tasks, API)
ABOVE a single `if config.feeds.dhan_enabled { …lane… }` wrapper, while preserving
the subscribe-before-publish ordering (obs/aggregator/tick-storage `.subscribe()`
BEFORE the lane processor publishes). The relocations are individually
order-safe (every shared item depends only on PROCESS-prefix values — proven), so
the hoist is TRACTABLE, but it is a ~9-block scatter-relocation + wrapping ~2,700
lines in an `if`, on the SEBI-critical live-trading boot. A single mis-placed
`.subscribe()` silently breaks zero-tick-loss; a double API bind breaks boot.

Per the operator's "correctness is paramount / STOP if you cannot complete the
hoist with provably-preserved ordering + a clean compile / do NOT ship a
semantics-changing half-refactor" directive (Rule 14), Stage 2 is deferred to a
focused follow-up rather than rushed in this pass. Stage 1 (the run-loop split)
is the load-bearing prerequisite D2b needs and is shippable + verified on its own.

---

## Edge Cases

| # | Edge case | Behaviour after hoist |
|---|---|---|
| 1 | Dhan-ON slow boot (prod default) | Shared infra built once in the hoisted section; lane built in the `if dhan_enabled` block; run-loop runs. Same tasks, reordered construction. |
| 2 | Dhan-OFF / Groww-only boot | Hoisted section builds API (incl. `/api/feeds`) + seal-writer + aggregator (Groww candles seal); lane skipped; run-loop runs. Replaces `run_shared_infra_only`. |
| 3 | Dhan-ON FAST boot (cache + market hours) | FAST arm matches FIRST and returns before the hoist — byte-identical, untouched. |
| 4 | Dhan-OFF, no Groww (no feed) | Same as edge 2; warn logged; shared infra + run-loop keep the process alive. |
| 5 | IP verify `return Ok(())` (live, IP mismatch) | Stays inside the lane block (Dhan-ON only). On Dhan-OFF the IP gate never runs (no Dhan calls) — OFF isolation preserved. The early `return` aborts the whole boot exactly as today; shared infra not yet relevant for OFF (OFF never reaches the IP gate). |
| 6 | QuestDB down at boot | seal-writer/API construct lazily (ILP lazy connect); aggregator idles. Dhan lane's QuestDB DDL surfaces BOOT-01/02 inside the lane. Same as today. |
| 7 | Docker auto-start disabled | Hoisted notifier/Docker section honours `infrastructure.auto_start_docker` exactly as the slow arm does today. |

---

## Failure Modes

| Failure | Result | Preserved-from-today? |
|---|---|---|
| Notifier strict-init fails | `Err` → REFUSE BOOT (systemd restarts). Hoisted section uses the SAME `initialize_strict` + coalescer logic the slow arm used. | yes |
| API bearer token SSM fetch fails | `?` propagates → boot fails (hard-fail, no env fallback). Same as slow arm today. | yes |
| API bind fails (port in use) | `?` propagates → boot fails. Now happens in the hoisted section instead of the slow arm; same outcome. | yes (moved, not changed) |
| Dhan auth fails | inside the lane block; same retry/halt semantics as today's slow arm. | yes |
| seal-writer construct fails | `error!` + continue (candles won't seal). Same as today. | yes |
| Run-loop teardown | `run_process_runloop` performs the SAME teardown order as `run_shutdown_fast` did (market-close → drain → WS abort → processor flush → partition detach → renewal/order-update/WS graceful/processor/trading/API abort → otel drop). | yes (refactored, same order) |

**Governing rule:** the hoist NEVER changes WHICH tasks run or their relative
publish/subscribe order — only the lexical construction site moves. Any ordering
that cannot be preserved → STOP + report (Rule 14), do not ship.

---

## Test Plan

Scoped to `crates/app` (`testing.md` block-scope rule).
- `cargo build -p tickvault-app` clean (0 warnings).
- `cargo test -p tickvault-app` green — especially `feed_toggle_lifecycle_guard.rs`
  and `per_feed_boot_isolation_guard.rs` (OFF-isolation invariants).
- `cargo fmt -p tickvault-app --check`.
- banned-pattern scanner + pub-fn-test + pub-fn-wiring guards exit 0.
- NEW guard test(s) in `crates/app/tests/`:
  - `dhan_on_and_off_share_one_shared_infra_construction` — source-scan: exactly
    ONE `axum::serve` + ONE `spawn_engine_b_aggregator` call reachable from BOTH
    the OFF and the ON-slow path (no duplicate `run_shared_infra_only`), and
    `run_shared_infra_only` no longer exists.
  - `aggregator_subscribe_precedes_dhan_tick_publish` — source-scan: the hoisted
    aggregator/observability `.subscribe()` site precedes the lane
    `run_tick_processor` publish site in `main()`.
  - Existing Step-C guards updated for the new structure (no longer assert the
    OFF path early-returns via `run_shared_infra_only`).
- `dhat` N/A (cold control-plane boot, not a hot path).
- Commit `.claude/hooks/.test-count-baseline` bump if net test count rises.

---

## Rollback

Single-PR, single-file (`crates/app/src/main.rs`) + test/guard additions.
Rollback = `git revert` the squash-merge commit. Because this is held as a DRAFT
and boot-tested by the operator before merge, a bad boot is caught pre-merge. The
FAST arm is byte-identical, so a fast-boot crash-recovery is unaffected by a
revert. No schema, no DEDUP-key, no Cargo.toml, no config changes — pure
control-flow refactor of one binary's boot.

---

## Observability

No new metrics/counters/audit tables (a pure construction-site move). The hoisted
section preserves every existing log line / `NotificationEvent` / Prometheus gauge
of the slow arm (`StartupComplete`, boot-timing Telegram, `tv_lifecycle_enricher_attached`,
`/health`, `/api/feeds`). The split run-loop preserves the `ShutdownInitiated` /
`Post-Market` notifications + partition-detach logs. The new guard tests are the
mechanical ratchet that the hoist stays single-construction + ordering-correct.

---

## Per-Item Guarantee Matrix (cross-ref)

Carries the 15-row + 7-row matrices of
`.claude/rules/project/per-wave-guarantee-matrix.md` by reference. This item is a
control-flow refactor: zero new tick-drop path (the broadcast/aggregator/processor
publish-subscribe order is preserved by construction — proven in Design §"Load-bearing
ordering"); no new hot-path allocation; the 2-WS Dhan lock + `dhan_disable_allowed`
gate untouched; no new WebSocket endpoint; composite-key/DEDUP unchanged (no table
edits). Honest envelope: behaviour-CHANGING reorder, boot-test-gated, fast arm
byte-identical, slow + OFF paths share one construction.
