# Implementation Plan: Dhan cold-start-from-a-fully-OFF boot (Item D2)

**Status:** DRAFT
**Date:** 2026-06-26
**Approved by:** pending (coordinator 3-agent adversarial review first)
**Branch:** `claude/dhan-cold-start-d2-design`
**Authority chain honored:** CLAUDE.md > `operator-charter-forever.md` >
`websocket-connection-scope-lock.md` (2-WS lock + PR-E Dhan runtime-toggle +
dry_run-only disable gate) > `design-first-wall.md` (the 6 sections below) >
`per-wave-guarantee-matrix.md` (15+7) > `audit-findings-2026-04-17.md` Rule 14
(no skeleton PR) / Rule 16 (shutdown = `Arc<Notify>`, never `CancellationToken`).

> **Scope guard:** docs-only design. NO `crates/*/src/*.rs` edits in this PR.
> This file is the design the implementation sub-PRs (D2a/D2b/D2c) build to.

---

## §0. Context — exactly what is broken, with file:line evidence

Everything else in the feed-toggle epic is merged (`active-plan-feed-followups.md`
Item D split D1=Groww/D2=Dhan; `active-plan-feed-toggle-full-lifecycle.md` PR-1..PR-4).
The ONE remaining residual is the **boot-OFF Dhan cold-start**, documented honestly
in `dhan_activation.rs:29-48`:

> "The FULL boot-OFF → cold-start of the ~4000-line inline Dhan boot spine
> (auth → daily-universe build → WS-pool spawn …) is NOT lifted out of `main()`
> by this PR. … Until it lands, a Dhan feed that was OFF *at boot* has no pool to
> start."

### The two behaviours TODAY (verified)

| Behaviour | Where | Status |
|---|---|---|
| Dhan-OFF boot returns early — the inline spine never runs | `crates/app/src/main.rs:1172-1189` (`if !config.feeds.dhan_enabled { … wait_for_shutdown_signal().await; return Ok(()); }`) | correct; keeps the runtime alive for Groww-only |
| `start_marks_running(false)` refuses to mark a phantom lane running | `crates/app/src/dhan_activation.rs:151-153` + `:220` gate; sentinel `dhan_pool_present` `feed_state.rs:75/123-135` | correct; closes the false-OK |
| Groww CAN cold-start at runtime (the working precedent) | `crates/app/src/groww_activation.rs:115-307` (`run_groww_activation_watcher` + `activate_groww_lane`) | the SHAPE D2 mirrors |
| **GAP:** a Dhan feed OFF at boot cannot be started at RUNTIME | the inline spine lives at `main.rs:1191`→`~5166` (fast `:1211-2055` + slow `:2057-5166`); it is NOT callable | **what D2 fixes** |

### The inline Dhan boot spine — discrete stages + threaded state (verified)

The spine has TWO arms behind `main.rs:1211` (`if let Some(cache_result) = fast_cache.filter(|_| is_market_hours)` → FAST boot, else SLOW boot). D2 extracts
the **SLOW arm** stages (the FAST arm is a crash-recovery optimization that reuses
the SAME building blocks; see §1 "Fast-path handling"). Slow-arm stages and the
state each threads:

| Stage | main.rs region | Threads / produces |
|---|---|---|
| Auth (TOTP→JWT, SSM, validate, deferred) | `:2057-2987` → `let token_manager` `:2147`, global set `:2993` | `Arc<TokenManager>` (owns `TokenHandle` arc-swap), `client_id` |
| QuestDB DDL / readiness | within `:2057-3010` (parallel `tokio::join!` `:2597`) | (idempotent; no lane-owned state) |
| Daily-universe build | `load_instruments(...)` (called both arms; slow path resolves `subscription_plan`, `fresh_universe`) | `Arc` subscription plan, universe |
| Main-feed WS pool create | `create_websocket_pool(...)` `:3032-3046` (dhan_flag `:3042`) | `pool_receiver` (mpsc), `ws_pool_ready` (handle) |
| Tick processor | `run_tick_processor(...)` `:3571`, handle `:3273` | `processor_handle` (JoinHandle) |
| Main-feed pool spawn | `pool.spawn_connections(...)` `:3643-3651` (+ `shutdown_notify` `:3627`) | `ws_handles`, `ws_pool_arc` |
| Order-update WS | `run_order_update_connection(...)` `:4691` (dhan_flag `:4686`) | `order_update_handle` |
| Token-renewal | `token_manager.spawn_renewal_task()` `:4848` (+ sweep `:4878`, watchdog `:4860`) | `renewal_handle` |
| Shutdown | `run_shutdown_fast(ws_handles, processor_handle, renewal_handle, order_update_handle, api_handle, trading_handle, …, ws_pool_arc, shutdown_notify, …)` `:5151-5165` | consumes all the handles |

The dhan_flag wiring points the operator named — `:1377` (fast pool), `:1833`
(fast order-update), `:3042` (slow pool), `:4686` (slow order-update) — are the
4 sites where the shared `Arc<AtomicBool>` Dhan enable flag is handed to the
`core` WS loops for PR-E in-loop dormancy. They stay; D2 adds the lifecycle
(create/teardown) the dormancy alone cannot do for a never-spawned pool.

### Why this is hard (honest, anti-pattern Rule 14)

The spine is ~4000 inline lines on the SEBI-critical trading path, threading
~9 pieces of state through `main()`'s local scope (auth → token_manager →
universe → pool → processor → order-update → renewal → shutdown). It is NOT a
pure-function lift. The risk is a **behaviour-changing refactor** of the boot
path. D2 therefore splits into a **behaviour-identical extraction (D2a)** before
any new runtime path is wired (D2b), with the guard/observability ratchets last
(D2c). No skeleton, no `// TODO`, no inert pub surface (Rule 14): D2a's extracted
fn has its REAL boot call site from line 1; D2b's runtime caller is the dormant
watcher already spawned at `:377`.

---

## 1. Design

### 1.1 The `LaneState` FSM

A 5-state lane FSM, owned by the Dhan activation watcher, replaces today's
2-flag (`dhan_lane_running` + `dhan_pool_present`) ad-hoc model with an explicit
single source of truth. States and the ONLY legal transitions:

```
            start_dhan_lane() begins
   ┌────────────────────────────────────────────────┐
   │                                                  ▼
 Off ──desired ON──▶ Starting ──all stages ok──▶ Running
   ▲                    │   │                        │
   │                    │   │ start fails / cancelled │ desired OFF
   │            cancel  │   │ (auth/universe/pool)    │ (gate permits)
   │          mid-start │   ▼                        ▼
   └────────◀───────────┴── Off ◀──teardown done── Stopping
```

| State | Meaning | `dhan_lane_running` reported |
|---|---|---|
| `Off` | no pool, no tasks; dormant watcher polling the flag | `false` |
| `Starting` | a `start_dhan_lane()` task is in-flight (auth→universe→pool) | `false` (no false-OK — not live until Running) |
| `Running` | pool spawned, ticks flowing, order-update + renewal up | `true` |
| `Stopping` | a `stop_dhan_lane()` teardown is in-flight | `false` |

**Legal transitions (exhaustive — anything else is a bug the FSM rejects):**
`Off→Starting`, `Starting→Running` (success), `Starting→Off` (start failed OR
cancelled mid-start), `Running→Stopping` (desired OFF + gate permits),
`Stopping→Off` (teardown complete). NO `Running→Starting` (idempotent — already
running), NO `Off→Stopping` (nothing to tear down), NO `Off→Running` (must pass
through Starting). The FSM is a pure `next_lane_state(current, event) ->
Option<LaneState>` returning `None` for an illegal transition (mirrors
`reconcile_lane_action`'s pure-and-total style so it is unit-testable without I/O).

This FSM is the boot-ON case too: the inline boot drives `Off→Starting→Running`
exactly as a runtime enable does (ONE code path for both, the Groww-watcher
property). Boot-ON byte-identical is preserved because the extracted
`start_dhan_lane()` (D2a) IS the inline spine — the boot path just calls it.

### 1.2 `start_dhan_lane(...)` / `stop_dhan_lane(...)` signatures

The functions live in a NEW module `crates/app/src/dhan_lane.rs` (NOT in
`dhan_activation.rs`, which stays the pure-decision watcher; the lane fns drive
live I/O). They are `async`, take a re-entrant **context struct** capturing the
shared state that today lives in `main()` locals, and a **cancellation token**
for cancel-safety.

```rust
/// Everything start_dhan_lane needs that today lives in main()'s scope.
/// Captured ONCE at boot (cheap Arc clones) so the lane is start-able
/// re-entrantly from the dormant watcher — not pinned inside main().
pub struct DhanLaneContext {
    config: Arc<AppConfig>,                  // immutable boot config
    feed_runtime: Arc<FeedRuntimeState>,     // the shared enable/UI/gate atomics
    notifier: NotificationService,           // Telegram (Clone)
    health_status: Arc<HealthStatus>,        // /health counters
    ws_frame_spill: WsFrameSpill,            // WAL→ring→spill→DLQ (shared, reused)
    trading_calendar: Arc<TradingCalendar>,
    tick_broadcast_sender: broadcast::Sender<…>,   // aggregator/trading subscribe
    order_update_sender: broadcast::Sender<…>,
    questdb: QuestDbConfig,
    // … the remaining cheap-Arc handles the spine threads
}

/// Handles a started lane owns, so stop_dhan_lane can tear them down.
/// Held by the watcher in an Option<DhanLaneHandles> for the ON period.
pub struct DhanLaneHandles {
    token_manager: Arc<TokenManager>,
    ws_pool_arc: Arc<WebSocketPool>,
    ws_handles: Vec<JoinHandle<()>>,
    processor_handle: JoinHandle<()>,
    order_update_handle: JoinHandle<()>,
    renewal_handle: JoinHandle<()>,
    lane_shutdown: Arc<Notify>,   // per-lane Notify (Rule 16) — NOT the process one
}

/// Cold-start the full Dhan lane. Drives: auth → daily-universe build →
/// main-feed WS pool create+spawn → tick processor → order-update WS →
/// token-renewal. Cancel-safe: every .await is reached only while the
/// CancellationGuard is live; on cancel it returns Err(Cancelled) AFTER
/// aborting any partially-spawned task. Returns the owned handles on success.
pub async fn start_dhan_lane(
    ctx: &DhanLaneContext,
    cancel: &CancelToken,          // Arc<Notify>-backed; watcher fires on disable
) -> Result<DhanLaneHandles, StartLaneError>;

/// Graceful teardown of a Running lane: close WS(es) via lane_shutdown,
/// stop tick processor, drop order-update + renewal, mark stopped. Idempotent
/// (Stopping→Off only). Does NOT touch the process-wide shutdown.
pub async fn stop_dhan_lane(handles: DhanLaneHandles, ctx: &DhanLaneContext);
```

**`StartLaneError`** is a typed enum (`AuthFailed`, `UniverseBuildFailed`,
`WsPoolSpawnFailed`, `Cancelled`) so each maps to a distinct ErrorCode (§3).

### 1.3 How the Dhan-OFF boot changes: early-return → dormant idle

Today `main.rs:1172-1189` early-returns on Dhan-OFF. D2b changes this to: build
the `DhanLaneContext` (cheap Arc captures of the already-constructed shared
infra), spawn the **already-present** dormant watcher (`:377`) handing it the
context + a `Some(ctx)`, then idle-await shutdown EXACTLY as today (Groww-only
keeps running). The watcher's reconcile loop, on observing desired-ON with a
context available and `LaneState::Off`, transitions `Off→Starting` and runs
`start_dhan_lane(ctx)`. This is the SAME unconditional-spawn + level-triggered
pattern the Groww watcher already proves (`main.rs:353-360`, `groww_activation.rs`).

Crucially: the dormant watcher is spawned BEFORE the Dhan-OFF dispatcher
(`main.rs:361-379`, already true today) so it survives the Groww-only branch.
D2b's only change to that branch is to give the watcher the `DhanLaneContext`
so it has something to start, and to delete `start_marks_running`'s "no pool ⇒
refuse" residual once a real cold-start exists (the sentinel `dhan_pool_present`
is subsumed by `LaneState != Off`).

### 1.4 Re-entrant state capture (instead of `main()` locals)

The spine today reads ~9 values from `main()`'s scope. D2a's mechanical move:
each stage's inputs become fields of `DhanLaneContext`, built once at boot from
the SAME constructed shared infra (observability, WAL spill, calendar, feed
runtime, notifier, health, questdb config) that is ALREADY up before the Dhan
dispatcher at `:1172`. Boot-ON: `main()` builds the ctx, calls
`start_dhan_lane(&ctx)` inline where the spine used to run, gets back
`DhanLaneHandles`, threads them into `run_shutdown_fast` as today. Boot-OFF: the
ctx is handed to the watcher; `main()` idles. Either way the inputs are the same
struct — no duplicated logic (Rule 14), no second auth path.

### 1.5 Reuse of existing resilience machinery (NO redesign)

`start_dhan_lane` REUSES, never reimplements:
- **WAL→ring→spill→DLQ:** `ws_frame_spill` is captured in the ctx and passed to
  `create_websocket_pool(..., ws_frame_spill.clone(), …)` exactly as `:3040`.
  Zero-tick-loss capture is identical to boot-ON.
- **`SubscribeRxGuard`** (subscription preservation across reconnects): lives
  inside the pool/`connection.rs`; untouched — the cold-started pool gets it for
  free.
- **PR-E in-loop dormancy:** the dhan_flag (`:3042`/`:4686`) is still handed to
  the WS loops, so once Running, a later OFF→ON flap inside the same lane uses
  the cheap in-loop pause/resume; only a Stopping→Off→Starting full teardown
  rebuilds the pool.
- **Token renewal + sweep + mid-session watchdog:** spawned by the lane via the
  same `token_manager.spawn_renewal_task()` (`:4848`) etc.

### 1.6 NO new WebSocket endpoint (2-WS lock confirmed)

`start_dhan_lane` spawns EXACTLY the same two Dhan WebSockets the inline spine
spawns: 1 main-feed (`create_websocket_pool` → `wss://api-feed.dhan.co`, pool
size constant 1) + 1 order-update (`wss://api-order-update.dhan.co`). It calls
the SAME `create_websocket_pool` / `run_order_update_connection` functions — no
new URL constant, no new endpoint, no second main-feed conn. The
`websocket-connection-scope-lock.md` 2-Dhan-WS lock and PR-E runtime-toggle
authorization both hold verbatim. D2 changes the lane LIFECYCLE (start/stop),
never the connection count, endpoints, or universe.

### 1.7 Fast-path handling (avoid scope creep)

The FAST boot arm (`:1211-2055`) is a market-hours crash-recovery optimization
that uses a cached token to skip SSM. D2a extracts the SLOW arm into
`start_dhan_lane` (the canonical cold path). The FAST arm is left
**byte-identical** in D2a — it is only ever reached at BOOT with a valid cache,
never from a runtime toggle, so a runtime cold-start always uses the slow/canonical
`start_dhan_lane`. (Folding the fast arm into the lane fn is a possible future
simplification, explicitly OUT OF SCOPE for D2 to keep the SEBI-spine diff minimal.)

---

## 2. Edge Cases

| # | Edge case | Behaviour |
|---|---|---|
| 1 | Rapid ON→OFF→ON flap (faster than 2s poll) | Level-triggered watcher (`reconcile_dhan_lane_action_with_gate`) converges to the FINAL desired value — no missed edge. If final=ON and state=Off → one `Off→Starting`. Proven pattern: `reconcile_dhan_lane_action_sub_poll_flap_converges_no_missed_edge`. |
| 2 | ON while already `Running` | Reconcile yields `None` (desired ON + activated) → no second `start_dhan_lane`, no double pool. Idempotent. FSM rejects `Running→Starting`. |
| 3 | OFF received mid-`Starting` (cancel) | Watcher fires the `CancelToken`; `start_dhan_lane` returns `Err(Cancelled)` at its next `.await`, having aborted any partially-spawned task. FSM `Starting→Off`. NO half-running lane (Rule 14). |
| 4 | Token expiry DURING cold-start | Auth stage uses the same `token_manager` deferred-auth path; a stale/failed token → `StartLaneError::AuthFailed` → `Starting→Off` + ErrorCode + Telegram. Retry on next poll per policy (bounded backoff like Groww's pull-until-success — see §3). |
| 5 | Daily-universe not yet built at runtime start | `start_dhan_lane` calls `load_instruments` itself (the universe build is INSIDE the lane, not assumed). If it fails → `UniverseBuildFailed` → `Starting→Off`. The existing INSTR-FETCH-* infinite-retry inside `load_instruments` is preserved. |
| 6 | QuestDB down at runtime start | DDL/readiness stage surfaces the existing BOOT-01/BOOT-02 path; lane stays `Starting` retrying (QuestDB absorb), or returns to `Off` after the boot deadline. The rescue ring buffers nothing yet (no pool) — no tick loss because no ticks are flowing pre-pool. |
| 7 | dry_run/open-orders gate refuses disable | A desired-OFF while `can_disable_dhan()==false` → `reconcile_dhan_lane_action_with_gate` downgrades `Stop→None`; FSM stays `Running`. Two-layer (handler CONFLICT + watcher) preserved. |
| 8 | OFF at boot, then ON before universe/auth ready | Watcher transitions `Off→Starting` and `start_dhan_lane` blocks on auth/universe internally (it OWNS those stages); the operator sees `Starting` (honest, not `Running`) until all stages complete. No false-OK. |
| 9 | ON at boot then OFF at runtime (the already-working PR-E case) | Within a Running lane, PR-E in-loop dormancy pauses the WS; the watcher's gated `Stop` then drives `Running→Stopping→Off` (full teardown) only if the operator wants the lane fully down. Backwards-compatible. |
| 10 | Dhan-OFF boot, Groww-only, never toggled | Watcher polls, state stays `Off` forever; zero Dhan work (no auth/universe/connect) — OFF-feed isolation preserved (the `per_feed_boot_isolation_guard.rs` invariant, strengthened to "no work while OFF"). |

---

## 3. Failure Modes

The governing rule (Rule 14 + operator charter §F): **a failure NEVER leaves a
half-running lane.** Every failed start returns the FSM to `Off`, emits a typed
ErrorCode at `error!` (routes to Telegram, Rule 5), and the watcher retries on a
bounded backoff. Partial work is aborted, not left dangling.

| Failure | Stage | FSM result | Code (new) | Operator signal |
|---|---|---|---|---|
| Auth ok, universe build fails | universe | `Starting→Off` | `DHAN-LANE-01` (`StartLaneError::UniverseBuildFailed`) | `error!` + Telegram + counter `tv_dhan_lane_start_failed_total{stage="universe"}`; INSTR-FETCH-* already covers the internal retry |
| WS pool spawn fails mid-way (some conns up, some fail) | pool spawn | abort spawned conns → `Starting→Off` | `DHAN-LANE-02` (`WsPoolSpawnFailed`) | `error!` + Telegram; the partial conns are aborted (no orphan sockets) |
| Auth fails (token/TOTP/SSM) | auth | `Starting→Off` | `DHAN-LANE-03` (`AuthFailed`) | reuse AUTH-GAP-* semantics; `error!` + Telegram; bounded retry |
| Cancel during `Stopping` leaves dangling tasks | teardown | the per-lane `lane_shutdown` `Notify` (Rule 16) is fired AND each handle is `.abort()`ed defensively; `stop_dhan_lane` awaits handles with a timeout then force-aborts | `DHAN-LANE-04` (`TeardownTimeout`, Severity::Medium) | `warn!`→`error!` if timeout; counter `tv_dhan_lane_teardown_forced_total` |
| Phantom `dhan_pool_present` (lane claims Running with no pool) | invariant | `start_marks_running` residual is REMOVED; the FSM's `Running` state is only set AFTER `start_dhan_lane` returns `Ok(handles)` — structurally impossible to be `Running` without a pool | n/a (closed by construction) | the existing `dhan_pool_present` false-OK test is rewritten as "Running ⟺ handles present" |
| Start task panics / early-returns before reaching `Running` | any | dead-activation watchdog (`is_dead_activation`) clears the handle → ONE bounded re-Start (mirrors WS-GAP-05) | reuse existing | `error!` "lane start task ended without bringing the lane up" |

**Cancel-safety mechanism (the load-bearing detail):** `start_dhan_lane` is one
owned task the watcher holds a `JoinHandle` for (exactly the Groww-watcher
shape, `groww_activation.rs:125,163-169,177-182`). On a disable transition the
watcher `.abort()`s that handle; because every stage runs INLINE inside the one
task (no detached children), the abort cancels the in-flight stage at its next
`.await`. Any task the lane HAD already spawned (a partial pool conn) is tracked
in a local `Vec<JoinHandle>` and aborted in the `start_dhan_lane` cancel/`Err`
cleanup path before returning — so cancel never leaks a spawned WS task.

---

## 4. Test Plan

Applicable categories from the 22 (`testing.md`), scoped to `crates/app`:
**unit** (FSM transitions), **integration** (lifecycle guards), **loom or
deterministic-sequence chaos** (toggle flap), **property** (FSM totality),
plus the mechanical **pub-fn-test** + **pub-fn-wiring** guards for every new
pub fn, and **dhat** is N/A (cold control-plane, not hot path — same honest
envelope as the lifecycle plan §244).

### FSM unit tests (`dhan_lane.rs` / `dhan_activation.rs`)
- `next_lane_state_off_to_starting_on_start`
- `next_lane_state_starting_to_running_on_success`
- `next_lane_state_starting_to_off_on_failure`
- `next_lane_state_starting_to_off_on_cancel`
- `next_lane_state_running_to_stopping_on_disable`
- `next_lane_state_stopping_to_off_on_teardown`
- `next_lane_state_rejects_running_to_starting` (idempotent ON)
- `next_lane_state_rejects_off_to_running` (must pass Starting)
- `next_lane_state_rejects_off_to_stopping` (nothing to tear down)
- `next_lane_state_is_total` (property: every (state,event) returns Some|None, never panics)

### Cancel-safety tests
- `start_dhan_lane_returns_cancelled_when_token_fired_before_first_await`
- `start_dhan_lane_aborts_partial_pool_conns_on_cancel` (no leaked JoinHandle)
- `stop_dhan_lane_is_idempotent_stopping_to_off`
- `cancel_during_stopping_force_aborts_after_timeout`

### Toggle-flap / chaos (extend `feed_toggle_lifecycle_guard.rs`)
- `dhan_cold_start_storm_converges_to_final_on` (deterministic level-sequence)
- `dhan_cold_start_storm_no_double_start`
- `dhan_cold_start_off_at_boot_then_on_reaches_running`
- `dhan_cold_start_cancel_mid_starting_returns_to_off`
- loom model (if cheap) `dhan_lane_fsm_no_lost_transition_under_interleaving`

### Boot-isolation (extend `per_feed_boot_isolation_guard.rs`)
- `off_dhan_feed_does_zero_work_until_first_on` (no auth/universe/connect while Off)
- `dhan_off_boot_no_longer_early_returns_before_watcher_has_context` (source-scan)

### Wiring/mechanical
- `pub-fn-wiring-guard.sh`: `start_dhan_lane` call site = inline boot (D2a) +
  the watcher (D2b); `stop_dhan_lane` call site = watcher disable + `run_shutdown_fast`.
- `pub-fn-test-guard.sh`: every new pub fn has a `#[test]` or `// TEST-EXEMPT:`.
- `error_code_rule_file_crossref.rs`: every new `DHAN-LANE-0N` variant mentioned
  in a rule file (new `.claude/rules/project/dhan-lane-error-codes.md`).
- `error_code_tag_guard.rs`: every `error!` carries `code = ErrorCode::X.code_str()`.

### Per-PR commands
`cargo test -p tickvault-app --lib --tests`, `cargo test -p tickvault-api --lib`
(feed_state), `cargo clippy --workspace -- -D warnings -W clippy::perf`,
`cargo fmt --check`, the 4 hooks above, 3-agent adversarial review each PR.

---

## 5. Rollback

A config feature flag `feeds.dhan_runtime_cold_start` (default **false** in D2a,
flipped **true** in D2b) gates the NEW runtime-start path:
- **flag false** → the Dhan-OFF boot behaves EXACTLY as today: early-return at
  `main.rs:1172-1189` (or the dormant watcher refuses to cold-start, logging the
  existing "restart with `dhan_enabled=true` required" INFO). Boot-ON is always
  byte-identical regardless of the flag (it calls `start_dhan_lane` inline, which
  IS the old spine).
- **flag true** → the watcher drives `Off→Starting→Running` on a runtime enable.

The flip-to-off is itself a TESTED code path:
`dhan_cold_start_flag_off_falls_back_to_boot_time_only` asserts that with the
flag false, a runtime Dhan enable on a boot-OFF run records the desired flag but
keeps `LaneState::Off` (no cold-start) — the documented pre-D2 behaviour. Each
sub-PR is independently `git revert`-able; D2a (pure extraction) reverts to the
inline spine, D2b reverts to "watcher refuses cold-start", D2c reverts the guards.

---

## 6. Observability

7-layer telemetry for EACH new lifecycle transition (edge-triggered, Rule 4;
market-hours-aware where the transition implies live data, Rule 3):

| Layer | Artefact |
|---|---|
| 1. Counter | `tv_dhan_lane_transitions_total{from,to}` (edge — one per transition); `tv_dhan_lane_start_failed_total{stage}`; `tv_dhan_lane_teardown_forced_total` |
| 2. Gauge | `tv_dhan_lane_state` (0=Off,1=Starting,2=Running,3=Stopping) — single-glance lane state for the dashboard + `/feeds` page |
| 3. Tracing span | `#[instrument]` on `start_dhan_lane` / `stop_dhan_lane` (cold path — span overhead irrelevant) |
| 4. `error!` w/ code | `DHAN-LANE-01..04` with `code = ErrorCode::X.code_str()` on every failure (tag-guard enforced) |
| 5. Telegram | new `NotificationEvent::DhanLaneStarted` (Info) / `DhanLaneStartFailed { stage, reason }` (High) / `DhanLaneStopped` (Info) — auto-driver wording, Severity-routed |
| 6. Audit row | `dhan_lane_audit` QuestDB table, DEDUP `(trading_date_ist, transition_kind, ts)` (I-P1-11-style; transition-chain rows both survive per the Phase-0 audit-table template) |

Edge-trigger: a transition fires its counter/Telegram ONCE on the edge, not per
poll. The `Starting`→`Running` success emits the positive signal (no false-OK —
only after `start_dhan_lane` returns Ok). `tv_dhan_lane_state` gives the operator
a real-time "is the lane Off/Starting/Running?" without inferring from logs.

---

## Per-Item Guarantee Matrix (15-row + 7-row)

This plan and EVERY sub-PR (D2a/D2b/D2c) carry the full 15-row "100% everything"
matrix and the 7-row Resilience Demand matrix — cross-referenced from
`.claude/rules/project/per-wave-guarantee-matrix.md` (all 15 + all 7 rows apply
verbatim). Highlights specific to D2:

- **Audit coverage:** new `dhan_lane_audit` table (Layer 6 above).
- **Code performance / O(1):** N/A axis — cold control-plane; the hot tick path
  (already O(1)) is UNTOUCHED. `start_dhan_lane` is a boot-equivalent cold path.
- **Monitoring/alerting:** the 7-layer telemetry (§6) + a CloudWatch alarm on
  `tv_dhan_lane_start_failed_total > 0`.
- **Bugs/code review:** 3-agent adversarial review (hot-path + security +
  hostile) on EACH sub-PR diff, before AND after impl (charter §E).
- **Security:** the dry_run-only disable gate (`can_disable_dhan`) is UNCHANGED;
  no token in logs (the `start_dhan_lane` error paths redact via
  `capture_rest_error_body`, mirroring `main.rs:1336-1338`).
- **Resilience (7-row):** zero-tick-loss WAL/ring/spill/DLQ reused unchanged;
  `SubscribeRxGuard` reused unchanged; composite-key dedup unaffected; 2-WS lock
  held; real-time proof via §6 telemetry + the lifecycle guards.

---

## Honest envelope (mandatory wording, charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: D2 makes
> a boot-OFF Dhan feed cold-startable at runtime by extracting the inline boot
> spine into a callable `start_dhan_lane()` driven by the existing dormant
> watcher — reusing the SAME WAL→ring→spill→DLQ + `SubscribeRxGuard`
> zero-tick-loss machinery, adding NO WebSocket endpoint (the 2-Dhan-WS lock
> holds), and preserving the dry_run-only `can_disable_dhan` safety gate. A
> failed cold-start (auth/universe/pool) returns the lane to `Off` with a typed
> ErrorCode + Telegram — NEVER a half-running lane. The lane FSM
> (`Off→Starting→Running→Stopping→Off`) is pure-unit-tested for totality + legal
> transitions; cancel-safety and toggle-storm convergence are ratcheted by
> deterministic guards. Boot-ON is byte-identical (the boot calls the same
> extracted fn). NOT claimed: that the fast-boot crash-recovery arm is folded in
> (out of scope), nor that every conceivable Dhan-side failure self-heals — the
> retry envelope is bounded, beyond it the lane stays `Off` and the operator is
> paged."

---

## Implementation sub-PR breakdown (serial, each independently reviewable)

| Sub-PR | Scope | Risk | Behaviour change |
|---|---|---|---|
| **D2a** | **Pure extraction** of the slow-arm Dhan boot spine into `crates/app/src/dhan_lane.rs::start_dhan_lane` + `DhanLaneContext`/`DhanLaneHandles`. The boot path calls it inline EXACTLY where the spine ran; `run_shutdown_fast` receives the same handles. Add `stop_dhan_lane` (called only by `run_shutdown_fast` initially). Flag `feeds.dhan_runtime_cold_start` added, default **false** (no runtime path yet). | Medium (SEBI-spine refactor) but **behaviour-identical** | NONE — boot-ON byte-identical; boot-OFF still early-returns. Verified by snapshot/diff + full `cargo test`. |
| **D2b** | **Wire the runtime cold-start path + FSM.** Add `LaneState` + `next_lane_state` to `dhan_activation.rs`; give the dormant watcher (`main.rs:377`) the `DhanLaneContext`; on `Off`+desired-ON drive `Off→Starting`→`start_dhan_lane`→`Running`; on gated disable drive `Running→Stopping`→`stop_dhan_lane`→`Off`. Change the Dhan-OFF boot from early-return to "build ctx → hand to watcher → idle". Flip flag default **true**. Remove the `start_marks_running` "no pool" residual (subsumed by `LaneState`). | High (the actual new behaviour) | NEW: boot-OFF Dhan is now runtime-startable. Gated by the flag for rollback. |
| **D2c** | **Tests + observability.** All §4 FSM/cancel/chaos/wiring tests; the §6 7-layer telemetry (counters, `tv_dhan_lane_state` gauge, spans, `DHAN-LANE-01..04` ErrorCodes + `dhan-lane-error-codes.md` rule file, Telegram events, `dhan_lane_audit` table); CloudWatch alarm. Strengthen `per_feed_boot_isolation_guard.rs` + `feed_toggle_lifecycle_guard.rs`. | Low (additive) | NONE (observability + guards only). |

Serial per `pr-completion-protocol.md`: D2a merges to main → D2b rebases → D2c
rebases. Each carries the 15+7 matrix, the honest envelope, and the 3-agent review.
