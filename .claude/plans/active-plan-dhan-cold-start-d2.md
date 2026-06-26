# Implementation Plan: Dhan cold-start-from-a-fully-OFF boot (Item D2)

**Status:** DRAFT
**Date:** 2026-06-26
**Approved by:** pending (coordinator 3-agent adversarial review folded in — see §0.5)
**Branch:** `claude/dhan-cold-start-d2-design`
**Authority chain honored:** CLAUDE.md > `operator-charter-forever.md` >
`websocket-connection-scope-lock.md` (2-WS lock + PR-E Dhan runtime-toggle +
dry_run-only disable gate) > `design-first-wall.md` (the 6 sections below) >
`per-wave-guarantee-matrix.md` (15+7) > `audit-findings-2026-04-17.md` Rule 14
(no skeleton PR) / Rule 16 (shutdown = `Arc<Notify>`, never `CancellationToken`).

> **Scope guard:** docs-only design. NO `crates/*/src/*.rs` edits in this PR.
> This file is the design the implementation sub-PRs (D2-pre/D2a/D2b/D2c/D2d)
> build to.

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

### THE CORE CORRECTION (adversarial review, 2026-06-26) — the premise was FALSE

The previous DRAFT assumed "all shared infra is already up before the Dhan
dispatcher, so D2 is a pure extraction of the Dhan spine." **That premise is
FALSE and is the root cause of 4 CRITICAL + 4 HIGH findings.** Verified against
`main.rs`:

| Infra the previous draft assumed was "already up / shared" | Actual location (verified) | Consequence |
|---|---|---|
| HTTP **API server** (`axum::serve`) — incl. the `/api/feeds` toggle endpoint | spawned ONLY inside the Dhan block: fast `main.rs:~1978`, slow `:~4839` | **CRITICAL-1:** in Dhan-OFF mode there is NO API server → the `/api/feeds` endpoint the runtime-ON command depends on DOES NOT EXIST. The cold-start is unreachable. |
| **Tick broadcast** sender (`tick_broadcast_sender`) | inside the Dhan block | Groww + aggregator subscribe to it; it must outlive Dhan ON/OFF |
| **21-TF aggregator + seal-writer** (`global_seal_sender`, `spawn_seal_writer_loop` `:~1915`, `set_global_seal_sender` `:~2668`, `spawn_engine_b_aggregator` `:~3238`) | inside the Dhan block; `global_seal_sender` is a **process-wide singleton** (OnceLock-style "first wins") | **CRITICAL-2:** Groww depends on these. They are NOT lane state. A lane start/stop must NEVER create/tear them down. |
| `shutdown_notify` | created **twice** inside the Dhan block (`:~1460` fast, `:~3627` slow) + a 3rd process notify elsewhere | **HIGH-7:** see watchdog finding below |
| The **main run-loop** (market-close timer, partition-detach, `wait_for_shutdown_signal`) | lives INSIDE `run_shutdown_fast` (`:6699-6930`) — which is misnamed; it is NOT a teardown fn | **CRITICAL-3:** `stop_dhan_lane` must NOT reuse it |
| Pool **watchdog** that calls `std::process::exit(2)` on Halt | `:~6197` | **HIGH-7:** wired to the process notify → a lane-only stop won't silence it → false Halt kills the WHOLE process (and Groww with it) |

**Therefore D2 is NOT a pure extraction.** It requires a **shared-infra HOIST
first** (new sub-PR **D2-pre**) that lifts the process-shared singletons OUT of
the Dhan block so they run regardless of Dhan ON/OFF. Only after the hoist can
the Dhan spine be extracted (D2a) and made runtime-startable (D2b).

### The inline Dhan boot spine — discrete stages + threaded state (verified)

The spine has TWO arms behind `main.rs:1211` (`if let Some(cache_result) = fast_cache.filter(|_| is_market_hours)` → FAST boot, else SLOW boot). D2a extracts
the **SLOW arm** stages; the FAST arm is a crash-recovery optimization unified in
D2d (see §1.8). Slow-arm stages and the state each threads:

| Stage | main.rs region | Threads / produces | LANE-owned vs PROCESS-shared |
|---|---|---|---|
| Auth (TOTP→JWT, SSM, validate, deferred) | `:2057-2987` → `let token_manager` `:2147`, global set `:2993` | `Arc<TokenManager>`, `client_id` | **LANE** (owns the JWT; see CRITICAL-4) |
| QuestDB DDL / readiness | within `:2057-3010` (parallel `tokio::join!` `:2597`) | idempotent; no lane state | PROCESS |
| Daily-universe build | `load_instruments(...)` (slow path resolves `subscription_plan`, `fresh_universe`) | `Arc` subscription plan, universe | LANE input |
| Main-feed WS pool create | `create_websocket_pool(...)` `:3032-3046` (dhan_flag `:3042`) | `pool_receiver` (mpsc), `ws_pool_ready` | **LANE** |
| Tick processor | `run_tick_processor(...)` `:3571`, handle `:3273` | `processor_handle` | **LANE** (consumes the Dhan pool's frames) |
| Main-feed pool spawn | `pool.spawn_connections(...)` `:3643-3651` | `ws_handles`, `ws_pool_arc` | **LANE** |
| **Pool watchdog** | `spawn_pool_watchdog_task` (Halt → `std::process::exit(2)` `:~6197`) | watchdog handle | **LANE** (must be lane-scoped — HIGH-7) |
| Order-update WS | `run_order_update_connection(...)` `:4691` (dhan_flag `:4686`) | `order_update_handle` | **LANE** |
| Mid-session profile watchdog | `:~4853` | watchdog handle | **LANE** |
| Token-renewal + sweep | `token_manager.spawn_renewal_task()` `:4848` (+ sweep `:4878`, watchdog `:4860`) | `renewal_handle` | **LANE** |
| **Seal-writer / 21-TF aggregator** | `spawn_seal_writer_loop` `:~1915`, `spawn_engine_b_aggregator` `:~3238`, `global_seal_sender` singleton `:~2668` | `global_seal_sender` | **PROCESS-SHARED** (hoisted by D2-pre; Groww depends) |
| **API server** | `axum::serve` `:~1978`/`:~4839` | `api_handle` | **PROCESS-SHARED** (hoisted by D2-pre) |
| **`tick_broadcast_sender`** | inside the block | broadcast Sender | **PROCESS-SHARED** (hoisted by D2-pre) |
| Main run-loop / "shutdown" | `run_shutdown_fast(...)` `:6699-6930` (market-close timer, partition-detach, `wait_for_shutdown_signal`) | consumes handles | **PROCESS** run-loop (hoisted by D2-pre; NOT a lane teardown) |

The dhan_flag wiring (`:1377` fast pool, `:1833` fast order-update, `:3042` slow
pool, `:4686` slow order-update) are the 4 sites where the shared
`Arc<AtomicBool>` Dhan enable flag is handed to the `core` WS loops for PR-E
in-loop dormancy. They stay; D2 adds the lifecycle (create/teardown) the dormancy
alone cannot do for a never-spawned pool.

### Why this is hard (honest, anti-pattern Rule 14)

The spine is ~4000 inline lines on the SEBI-critical trading path, AND it is
tangled with process-shared infra (API server, seal-writer, aggregator, run-loop)
that must survive Dhan OFF. It is NOT a pure-function lift. The risk is a
**behaviour-changing refactor** of the boot path. D2 therefore sequences:
**D2-pre** (behaviour-identical hoist of shared infra out of the Dhan block) →
**D2a** (behaviour-identical extraction of the now-isolated Dhan spine into
`start_dhan_lane`, flag default false) → **D2b** (the new runtime path + FSM +
lane-scoped watchdog/notify + teardown-only `stop_dhan_lane`) → **D2c** (tests +
observability) → **D2d** (fast-arm unification). No skeleton, no `// TODO`, no
inert pub surface (Rule 14): every extracted fn has its REAL call site from
line 1.

---

## §0.5. Adversarial review findings folded in (4 CRITICAL + 4 HIGH)

The coordinator's 3-agent adversarial review (hot-path + security + hostile)
found that the previous DRAFT's "pure extraction" premise was structurally
false. Each finding and the concrete plan change that resolves it:

### CRITICAL

| # | Finding (verified file:line) | Resolution in this plan |
|---|---|---|
| **C1** | **No HTTP API server in Dhan-OFF mode.** `api_handle`/`axum::serve` spawn only inside the Dhan block (fast `:~1978`, slow `:~4839`). The `/api/feeds` toggle endpoint the runtime-ON command needs does NOT exist when Dhan is OFF. | **New first sub-PR D2-pre** hoists the API server (+ `FeedRuntimeState` routes), `tick_broadcast_sender`, the 21-TF aggregator + seal-writer (`global_seal_sender`), and the main run-loop OUT of the Dhan block into PROCESS-shared infra that runs regardless of Dhan ON/OFF. Behaviour-identical refactor; prerequisite for ANY runtime cold-start. §1.0 + §6 (sub-PR table). |
| **C2** | **Seal-writer / aggregator / `global_seal_sender` are process-shared singletons, not lane state** (`:~1915, ~2668, ~3238`). Groww depends on them. | Ownership model corrected (§1.1): seal-writer + aggregator + `tick_broadcast_sender` + API server = **PROCESS-SHARED** (owned by `main`/D2-pre, never by the lane). `start_dhan_lane`/`stop_dhan_lane` MUST NOT create or tear them down — only the Dhan WS pool + order-update + token-renewal + Dhan watchdogs are LANE-owned. |
| **C3** | **`run_shutdown_fast` is the main run-loop, not a teardown fn** (`:6699-6930`: market-close timer, partition-detach, `wait_for_shutdown_signal`). | `stop_dhan_lane` uses a NEW small teardown-only fn `teardown_dhan_lane_tasks` (renewal → order-update → graceful-WS → processor-flush ONLY) — never partition-detach, never `wait_for_shutdown_signal`. All "mirror run_shutdown_fast order" language removed. §1.4. |
| **C4** | **Token `OnceLock` race** — `set_global_token_manager` is a `OnceLock` (`token_manager.rs:58`, silently no-ops if already set). A runtime cold-start that re-runs auth builds a fresh `Arc<TokenManager>` + new JWT, invalidating the prior token (one-active-token rule), while the OnceLock keeps the OLD manager → renewal renews the wrong token → silent auth death. | The lane OWNS its `TokenManager` (field of `DhanLaneHandles`), dropped on Stop. The lane does NOT rely on the global `OnceLock` for its working token; the WS sleep-wake `force_renewal_if_stale` path reads the lane manager via the `DhanLaneContext`, not the stale global. Ratchet `cold_start_does_not_double_set_global_token_manager`. §1.3 + §3. |

### HIGH

| # | Finding (verified file:line) | Resolution in this plan |
|---|---|---|
| **H5** | **Disable-gate sampled, not held, across teardown** — an order could open mid-teardown after the gate was last checked. | `stop_dhan_lane` RE-ASSERTS `can_disable_dhan()` immediately before the irreversible WS-close; if the gate closed, ABORT teardown + return FSM to `Running`. Test `disable_aborts_if_orders_open_mid_teardown`. §2 (edge 11) + §3 + §4. |
| **H6** | **Double-pool on Start-during-Stopping.** | `Stopping→Off` is set ONLY after teardown awaits ALL WS/order-update handles join (NOT on Notify-fire). FSM REJECTS `Stopping→Starting`; Start is legal ONLY from a confirmed-empty `Off`. Chaos test `no_double_pool_during_teardown`. §1.2 + §4. |
| **H7** | **Three `shutdown_notify` + process-exiting pool watchdog** (`:~1460, ~3627`; watchdog `std::process::exit(2)` `:~6197`). A lane-only stop that fires a lane Notify won't silence the process-wired watchdog → false Halt → whole process exits, killing Groww. | The Dhan pool watchdog becomes **LANE-SCOPED** (created/torn-down by the lane, shares the lane's `Notify`) and its Halt path no longer calls `std::process::exit` — it signals the lane FSM (`Running→Stopping→Off` or bounded re-Start), leaving Groww + the process alive. §1.5 + §3. |
| **H8** | **Cancel mid-`Starting` leaks ~6 task families** — the previous draft only aborted pool conns. | §3 enumerates EVERY spawn reachable in the slow arm and classifies each shared-vs-lane (the seal-writer/aggregator/API are PROCESS — NOT aborted; the Dhan pool conns, order-update, renewal, sweep, mid-session profile watchdog, pool watchdog are LANE — ALL aborted on cancel). The `start_dhan_lane` cancel cleanup aborts every LANE-owned handle it has spawned so far, tracked in a `Vec<AbortOnDrop>`. §1.7 + §3. |

### MEDIUM / LOW also folded in

- **M9 — `LaneState` is an ATOMIC in `FeedRuntimeState`**, not a watcher-local var, so the boot-ON inline start and the watcher FSM observe the SAME state. The `dhan_pool_present` sentinel is removed ONLY once `LaneState` lives in shared state. §1.2.
- **L10 — fast-arm divergence:** D2a extracts the slow arm only; tracked follow-up **D2d** unifies the fast crash-recovery arm. PLUS a guard test asserting a runtime toggle can NEVER enter the fast arm. §1.8 + §4.
- **L11 — `DhanLaneContext` field completeness:** the struct in §1.3 lists EVERY field (no `…` elision). D2-pre's FIRST task is to produce this exhaustive list from a real compile (so nothing is missed). §1.3.
- **L12 — gauge / backoff / secret discipline:** `tv_dhan_lane_state` set ONLY on transition (edge-triggered, never per-poll); retry backoff cap pinned to Groww's `min(10 * 2^n, 300)`; `reason` strings in `dhan_lane_audit` + Telegram are FIXED enum discriminants, never raw auth body (no secret leak). Ratchet `dhan_lane_audit_has_no_secret_columns`. §6 + §3.

---

## 1. Design

### 1.0 D2-pre — the shared-infra HOIST (prerequisite, behaviour-identical)

**This is the new FIRST sub-PR and the load-bearing correction.** It lifts the
PROCESS-shared infra OUT of the Dhan block so it runs whether Dhan is ON or OFF.

```
                BEFORE D2-pre (today)                  AFTER D2-pre
   main()                                    main()
     ├─ shared infra (config, obs, spill…)     ├─ shared infra (config, obs, spill…)
     ├─ if dhan_enabled {                       ├─ tick_broadcast_sender         ◀ HOISTED
     │     API server          ◀ INSIDE         ├─ seal-writer + global_seal     ◀ HOISTED
     │     tick_broadcast       ◀ INSIDE        ├─ 21-TF aggregator (Engine B)   ◀ HOISTED
     │     seal-writer + aggr   ◀ INSIDE        ├─ API server + /api/feeds       ◀ HOISTED
     │     Dhan spine …                         ├─ if dhan_enabled { Dhan spine } │  (spine only)
     │     run-loop (run_shutdown_fast)         ├─ else { idle for Groww }        │
     │   } else { early-return }                └─ main run-loop                  ◀ HOISTED
```

Hoisted (now PROCESS-shared, constructed BEFORE the Dhan ON/OFF branch):

1. **`tick_broadcast_sender`** — the broadcast channel the aggregator + Groww +
   trading subscribe to. Constructed once; both feeds publish into it.
2. **Seal-writer + `global_seal_sender`** (`spawn_seal_writer_loop`,
   `set_global_seal_sender`) — the process-wide candle-seal singleton.
3. **21-TF aggregator (Engine B)** (`spawn_engine_b_aggregator` + heartbeat +
   IST-midnight force-seal) — consumes the broadcast, seals candles for BOTH
   feeds. Process-shared.
4. **API server** (`axum::serve`) + the `FeedRuntimeState` routes incl.
   `/api/feeds` — so the toggle endpoint EXISTS in Dhan-OFF mode (fixes C1).
5. **Main run-loop** — the market-close timer, partition-detach, and
   `wait_for_shutdown_signal` currently buried in `run_shutdown_fast`
   (`:6699-6930`) move to a clearly-named process run-loop that runs after EITHER
   the Dhan-ON spine OR the Dhan-OFF idle branch. `run_shutdown_fast` is split
   into (a) this run-loop and (b) a real graceful-shutdown teardown.

**Behaviour-identical guarantee:** D2-pre is a pure relocation — the SAME
functions are called in the SAME order; only their lexical position moves out of
the `if dhan_enabled` block. Boot-ON and boot-OFF observable behaviour is
byte-identical (verified by snapshot/log diff + full `cargo test`). No new
runtime path is wired in D2-pre.

### 1.1 Ownership model (PROCESS-shared vs LANE-owned) — the corrected classification

| Component | Owner | Created by | Torn down by | Lives across Dhan OFF? |
|---|---|---|---|---|
| config / observability / WAL spill / calendar / feed runtime / notifier / health / questdb | PROCESS | `main` | process exit | yes |
| `tick_broadcast_sender` | PROCESS | D2-pre | process exit | **yes** |
| seal-writer + `global_seal_sender` | PROCESS | D2-pre | process exit | **yes** (Groww needs it) |
| 21-TF aggregator (Engine B) + heartbeat + midnight force-seal | PROCESS | D2-pre | process exit | **yes** (seals Groww candles) |
| API server + `/api/feeds` routes | PROCESS | D2-pre | process exit | **yes** (C1 fix) |
| main run-loop (market-close, partition-detach, shutdown wait) | PROCESS | D2-pre | process exit | yes |
| `TokenManager` (Dhan JWT) | **LANE** | `start_dhan_lane` | `stop_dhan_lane` (drop) | no (C4 fix) |
| Dhan main-feed WS pool + conns | **LANE** | `start_dhan_lane` | `stop_dhan_lane` | no |
| Dhan tick processor | **LANE** | `start_dhan_lane` | `stop_dhan_lane` | no |
| order-update WS | **LANE** | `start_dhan_lane` | `stop_dhan_lane` | no |
| token-renewal + sweep + token watchdog | **LANE** | `start_dhan_lane` | `stop_dhan_lane` | no |
| Dhan **pool watchdog** (lane-scoped, no `process::exit`) | **LANE** | `start_dhan_lane` | `stop_dhan_lane` | no (H7 fix) |
| mid-session profile watchdog | **LANE** | `start_dhan_lane` | `stop_dhan_lane` | no |

**Invariant:** `start_dhan_lane` / `stop_dhan_lane` touch ONLY the LANE rows.
They NEVER construct or drop a PROCESS row. Ratchet:
`start_dhan_lane_does_not_spawn_seal_writer_or_aggregator_or_api`.

### 1.2 The `LaneState` FSM — stored as an atomic in `FeedRuntimeState` (M9)

A 5-state lane FSM is the single source of truth, stored as an
`AtomicU8`-backed `LaneState` field on the shared `FeedRuntimeState` (NOT a
watcher-local) so the boot-ON inline start AND the watcher FSM read/write the
same state. Replaces today's 2-flag (`dhan_lane_running` + `dhan_pool_present`).

```
            start_dhan_lane() begins
   ┌────────────────────────────────────────────────┐
   │                                                  ▼
 Off ──desired ON──▶ Starting ──all stages ok──▶ Running
   ▲                    │   │                        │
   │                    │   │ start fails / cancelled │ desired OFF + gate
   │            cancel  │   │ (auth/universe/pool)    │ re-asserted (§H5)
   │          mid-start │   ▼                        ▼
   └────────◀───────────┴── Off ◀──ALL handles join── Stopping
                                  (NOT on Notify-fire — §H6)
```

| State | Meaning | `dhan_lane_running` reported |
|---|---|---|
| `Off` | no pool, no Dhan tasks; dormant watcher polling the flag | `false` |
| `Starting` | a `start_dhan_lane()` task is in-flight (auth→universe→pool) | `false` (no false-OK) |
| `Running` | pool spawned, ticks flowing, order-update + renewal + lane watchdogs up | `true` |
| `Stopping` | a `stop_dhan_lane()` teardown is in-flight; pool conns NOT yet joined | `false` |

**Legal transitions (exhaustive — anything else the FSM rejects with `None`):**
`Off→Starting`, `Starting→Running`, `Starting→Off` (fail OR cancel),
`Running→Stopping` (desired OFF + gate permits), `Stopping→Off` (**only after
ALL WS/order-update/renewal handles have joined**, §H6). Explicitly REJECTED:
`Running→Starting` (idempotent), `Off→Stopping` (nothing to tear down),
`Off→Running` (must pass Starting), **`Stopping→Starting` (§H6 — no Start while a
teardown is draining; Start is legal ONLY from confirmed-empty `Off`)**. The FSM
is a pure `next_lane_state(current, event) -> Option<LaneState>` returning `None`
for an illegal transition (mirrors `reconcile_lane_action`'s pure-and-total style
so it is unit-testable without I/O).

This FSM is the boot-ON case too: the inline boot drives `Off→Starting→Running`
exactly as a runtime enable does (ONE code path for both, the Groww-watcher
property). Boot-ON byte-identical is preserved because the extracted
`start_dhan_lane()` (D2a) IS the (now-isolated) inline spine — the boot path just
calls it.

### 1.3 `DhanLaneContext` / `DhanLaneHandles` / lane fn signatures

The lane fns live in a NEW module `crates/app/src/dhan_lane.rs` (NOT in
`dhan_activation.rs`, which stays the pure-decision watcher). They are `async`,
take a re-entrant **context struct** (PROCESS-shared state captured as cheap Arc
clones) and a **cancellation token** (`Arc<Notify>`-backed, Rule 16).

> **L11 — exhaustive field list.** D2-pre's FIRST task is to compile `main()` and
> enumerate EVERY value the slow-arm spine reads from `main()`'s scope, turning
> each into a `DhanLaneContext` field. The list below is the design target; the
> implementation list is produced from a real compile (no `…` elision allowed).

```rust
/// PROCESS-shared state start_dhan_lane needs (all cheap Arc/Clone), captured
/// ONCE after D2-pre so the lane is start-able re-entrantly from the watcher.
pub struct DhanLaneContext {
    config: Arc<AppConfig>,                       // immutable boot config
    feed_runtime: Arc<FeedRuntimeState>,          // enable/UI/gate atomics + LaneState (M9)
    notifier: NotificationService,                // Telegram (Clone)
    health_status: Arc<HealthStatus>,             // /health counters
    ws_frame_spill: WsFrameSpill,                 // WAL→ring→spill→DLQ (PROCESS, reused)
    trading_calendar: Arc<TradingCalendar>,
    tick_broadcast_sender: broadcast::Sender<…>,  // PROCESS (D2-pre); lane PUBLISHES into it
    order_update_sender: broadcast::Sender<…>,    // PROCESS (D2-pre)
    questdb: QuestDbConfig,
    otel_provider: Option<Arc<SdkTracerProvider>>,// PROCESS — spans only, never torn down by lane
    prev_day_cache: Arc<PrevDayCache>,            // PROCESS
    audit_senders: AuditSenders,                  // boot/selftest/order/ws_event audit ILP senders (PROCESS)
    is_market_hours: Arc<AtomicBool>,             // PROCESS clock flags
    is_trading: Arc<AtomicBool>,
    feed_health: Arc<FeedHealthState>,            // PROCESS
    // … FINAL list produced by D2-pre's first task from a real compile of main()
}

/// Handles a started lane OWNS, so stop_dhan_lane can tear them down.
/// Held by the watcher in `Option<DhanLaneHandles>` for the ON period.
pub struct DhanLaneHandles {
    token_manager: Arc<TokenManager>,             // LANE-owned (C4) — dropped on Stop
    ws_pool_arc: Arc<WebSocketPool>,
    ws_handles: Vec<JoinHandle<Result<(), WebSocketError>>>,
    processor_handle: JoinHandle<()>,
    order_update_handle: JoinHandle<()>,
    renewal_handle: JoinHandle<()>,
    sweep_handle: JoinHandle<()>,
    token_watchdog_handle: JoinHandle<()>,
    pool_watchdog_handle: JoinHandle<()>,         // LANE-scoped, no process::exit (H7)
    mid_session_profile_handle: JoinHandle<()>,
    lane_shutdown: Arc<Notify>,                   // per-lane Notify (Rule 16) — NOT a process one
}

/// Cold-start the full Dhan lane. Drives ONLY lane-owned work: auth →
/// daily-universe build → main-feed WS pool create+spawn → tick processor →
/// order-update WS → token-renewal+sweep+watchdog → lane-scoped pool watchdog →
/// mid-session profile watchdog. Does NOT create the seal-writer/aggregator/
/// API server (PROCESS-shared, already up via D2-pre). Cancel-safe: every
/// LANE handle spawned so far is tracked and aborted on cancel/Err before
/// returning. Returns the owned handles on success.
pub async fn start_dhan_lane(
    ctx: &DhanLaneContext,
    cancel: &CancelToken,                         // Arc<Notify>-backed; watcher fires on disable
) -> Result<DhanLaneHandles, StartLaneError>;

/// Graceful teardown of a Running lane: RE-ASSERT can_disable_dhan() (H5) →
/// fire lane_shutdown → drop renewal/sweep/watchdogs → close order-update →
/// graceful-WS close → flush tick processor → drop token_manager (C4). AWAITS
/// all handles join before reporting Stopping→Off (H6). Calls
/// `teardown_dhan_lane_tasks` ONLY — never partition-detach, never
/// wait_for_shutdown_signal (C3). Returns Aborted if the gate re-closed.
pub async fn stop_dhan_lane(
    handles: DhanLaneHandles,
    ctx: &DhanLaneContext,
) -> StopLaneOutcome;     // Stopped | AbortedGateReclosed (→ FSM back to Running)
```

**`StartLaneError`** is a typed enum (`AuthFailed`, `UniverseBuildFailed`,
`WsPoolSpawnFailed`, `Cancelled`) so each maps to a distinct ErrorCode (§3).
**`TokenManager` is a field of `DhanLaneHandles`** (LANE-owned, C4) — the lane
does NOT depend on the global `OnceLock` for its working token.

### 1.4 `teardown_dhan_lane_tasks` — the NEW teardown-only fn (C3)

`stop_dhan_lane` calls a NEW small fn (NOT `run_shutdown_fast`):

```
teardown_dhan_lane_tasks(handles, lane_shutdown):
  1. fire lane_shutdown (Notify) — graceful WS close request
  2. abort + join: renewal_handle, sweep_handle, token_watchdog_handle,
     pool_watchdog_handle, mid_session_profile_handle
  3. abort + join: order_update_handle
  4. await graceful WS close: join ws_handles (bounded timeout, then force-abort)
  5. flush + join: processor_handle
  6. drop token_manager  (C4 — invalidates the lane JWT cleanly)
  -- NO partition-detach, NO market-close timer, NO wait_for_shutdown_signal --
```

`stop_dhan_lane` sets FSM `Stopping→Off` ONLY after step 4 confirms all WS
handles joined (H6). The PROCESS run-loop, seal-writer, aggregator, and API
server are UNTOUCHED — they keep running for Groww.

### 1.5 Lane-scoped pool watchdog (H7)

The Dhan pool watchdog is created INSIDE `start_dhan_lane`, shares the lane's
`lane_shutdown` Notify, and on a Halt condition:

- **does NOT call `std::process::exit(2)`** (the current `:~6197` behaviour),
- instead signals the lane FSM: `Running→Stopping→Off` (orderly lane teardown)
  OR a bounded re-Start (mirrors WS-GAP-05), leaving the process + Groww alive.

`stop_dhan_lane` aborts this watchdog as part of `teardown_dhan_lane_tasks`
step 2, so a lane stop silences it (no false Halt). Ratchet
`dhan_pool_watchdog_does_not_call_process_exit` (source-scan).

### 1.6 How the Dhan-OFF boot changes: early-return → dormant idle

Post-D2-pre, the API server + aggregator + seal-writer + run-loop are already up
before the Dhan ON/OFF branch. D2b changes `main.rs:1172-1189` from early-return
to: build the `DhanLaneContext`, hand it (as `Some(ctx)`) to the **already-present**
dormant watcher (`main.rs:377`), then fall into the SHARED run-loop (NOT a Dhan
early-return). The watcher's reconcile loop, on observing desired-ON + context
available + `LaneState::Off`, transitions `Off→Starting` and runs
`start_dhan_lane(ctx)`. SAME unconditional-spawn + level-triggered pattern the
Groww watcher already proves. The `start_marks_running` "no pool ⇒ refuse"
residual + the `dhan_pool_present` sentinel are removed ONLY once `LaneState`
lives in `FeedRuntimeState` (M9).

### 1.7 Cancel-safety + the ~6 lane task families (H8)

`start_dhan_lane` is ONE owned task the watcher holds a `JoinHandle` for (the
Groww-watcher shape). On a disable transition the watcher `.abort()`s that
handle; every stage runs INLINE in the one task, so abort cancels the in-flight
stage at its next `.await`. Tasks the lane HAD already spawned are tracked in a
local `Vec<AbortOnDrop>` and aborted in the cancel/`Err` cleanup before return.

The slow-arm spawns, classified (the cancel cleanup aborts ONLY the LANE rows;
PROCESS rows are never aborted because the lane never spawned them):

| Spawn | Owner | Aborted on cancel? |
|---|---|---|
| seal-writer loop (`:~1915`) | PROCESS | NO (D2-pre owns it) |
| 21-TF aggregator subscriber + heartbeat + midnight force-seal (`:~3238`) | PROCESS | NO |
| API server (`:~4839`) | PROCESS | NO |
| Dhan main-feed pool conns (`:3643`) | LANE | YES (tracked `Vec<AbortOnDrop>`) |
| tick processor (`:3571`) | LANE | YES |
| order-update WS (`:4691`) | LANE | YES |
| token-renewal + sweep + token watchdog (`:4848/:4878/:4860`) | LANE | YES |
| mid-session profile watchdog (`:~4853`) | LANE | YES |
| lane pool watchdog (`:~6197`, now lane-scoped) | LANE | YES |
| otel provider | PROCESS | NO (never torn down by lane) |

### 1.8 Fast-path handling + D2d (L10)

The FAST boot arm (`:1211-2055`) is a market-hours crash-recovery optimization
that uses a cached token to skip SSM. D2a extracts the **SLOW arm** into
`start_dhan_lane` (the canonical cold path). The FAST arm is left
**byte-identical** through D2a/D2b/D2c — it is only ever reached at BOOT with a
valid cache, never from a runtime toggle. **D2d** (tracked follow-up) unifies the
fast arm into the lane fn. PLUS a guard test
`runtime_toggle_never_enters_fast_arm` (source/behaviour-scan) asserts a runtime
cold-start ALWAYS uses the slow/canonical `start_dhan_lane`, never the fast arm.

### 1.9 Reuse of existing resilience machinery (NO redesign) + 2-WS lock

`start_dhan_lane` REUSES, never reimplements: WAL→ring→spill→DLQ (`ws_frame_spill`
in the ctx, passed to `create_websocket_pool` as `:3040`), `SubscribeRxGuard`
(inside the pool), PR-E in-loop dormancy (dhan_flag `:3042`/`:4686` still handed
to the WS loops), token renewal/sweep/watchdog. It spawns EXACTLY the same two
Dhan WebSockets the inline spine spawns — 1 main-feed (`wss://api-feed.dhan.co`,
pool size constant 1) + 1 order-update (`wss://api-order-update.dhan.co`) — via
the SAME `create_websocket_pool` / `run_order_update_connection`. No new URL, no
new endpoint, no second main-feed conn. The `websocket-connection-scope-lock.md`
2-Dhan-WS lock + PR-E runtime-toggle authorization both hold verbatim. D2 changes
the lane LIFECYCLE only.

---

## 2. Edge Cases

| # | Edge case | Behaviour |
|---|---|---|
| 1 | Rapid ON→OFF→ON flap (faster than 2s poll) | Level-triggered watcher converges to the FINAL desired value — no missed edge. final=ON & state=Off → one `Off→Starting`. |
| 2 | ON while already `Running` | Reconcile yields `None` (desired ON + activated) → no second start. FSM rejects `Running→Starting`. Idempotent. |
| 3 | OFF received mid-`Starting` (cancel) | Watcher fires `CancelToken`; `start_dhan_lane` returns `Err(Cancelled)` at next `.await`, having aborted every LANE handle it spawned (H8). FSM `Starting→Off`. NO half-running lane. |
| 4 | Token expiry DURING cold-start | Auth stage → `StartLaneError::AuthFailed` → `Starting→Off` + ErrorCode + Telegram + bounded backoff retry. |
| 5 | Daily-universe not yet built at runtime start | Lane calls `load_instruments` itself (universe build is INSIDE the lane). Fail → `UniverseBuildFailed` → `Starting→Off`. INSTR-FETCH-* infinite-retry preserved. |
| 6 | QuestDB down at runtime start | DDL/readiness surfaces BOOT-01/BOOT-02; lane stays `Starting` retrying or returns to `Off` after boot deadline. No tick loss (no pool yet). |
| 7 | dry_run/open-orders gate refuses disable | desired-OFF while `can_disable_dhan()==false` → reconcile downgrades `Stop→None`; FSM stays `Running`. |
| 8 | OFF at boot, then ON before universe/auth ready | `Off→Starting`; lane blocks on auth/universe internally; operator sees `Starting` (honest) until all stages complete. No false-OK. |
| 9 | ON at boot then OFF at runtime (PR-E case) | Within a Running lane, PR-E in-loop dormancy pauses the WS; gated `Stop` then drives `Running→Stopping→Off` (full teardown) only if the operator wants the lane fully down. |
| 10 | Dhan-OFF boot, Groww-only, never toggled | Watcher polls, state stays `Off`; zero Dhan work. PROCESS infra (API server, aggregator, seal-writer) STILL up → Groww candles seal + `/api/feeds` reachable (C1). |
| **11** | **Order opens DURING `stop_dhan_lane` teardown (H5)** | `stop_dhan_lane` RE-ASSERTS `can_disable_dhan()` immediately before the irreversible WS-close. If the gate has re-closed (an order/position opened mid-teardown), ABORT teardown, leave the already-up handles intact, FSM returns `Stopping→Running`. The system can never be blinded mid-trade. |
| **12** | **Start requested while `Stopping` (H6)** | FSM rejects `Stopping→Starting`. The watcher holds the desired-ON; once teardown awaits all handles join and reaches confirmed-empty `Off`, the NEXT poll drives a fresh `Off→Starting`. NO double pool. |
| **13** | **Cold-start re-runs auth → fresh JWT vs stale `OnceLock` (C4)** | The lane builds + OWNS its `TokenManager` (in `DhanLaneHandles`); renewal/force-renewal read the lane manager via the ctx, NOT the global `OnceLock`. The prior boot's global manager (if any) is irrelevant to the lane — no "renew the wrong token" death. On Stop, the lane manager is dropped. |
| **14** | **Lane pool watchdog Halt fires while Groww running (H7)** | Lane watchdog signals the lane FSM (teardown or bounded re-Start), NOT `std::process::exit`. Process + Groww survive. |

---

## 3. Failure Modes

Governing rule (Rule 14 + charter §F): **a failure NEVER leaves a half-running
lane, and NEVER tears down PROCESS-shared infra.** Every failed start returns the
FSM to `Off`, emits a typed ErrorCode at `error!` (→ Telegram, Rule 5), bounded
backoff retry. Partial LANE work is aborted; PROCESS infra is untouched.

| Failure | Stage | FSM result | Code (new) | Operator signal |
|---|---|---|---|---|
| Auth ok, universe build fails | universe | `Starting→Off` | `DHAN-LANE-01` (`UniverseBuildFailed`) | `error!` + Telegram + `tv_dhan_lane_start_failed_total{stage="universe"}`; INSTR-FETCH-* covers internal retry |
| WS pool spawn fails mid-way | pool spawn | abort spawned LANE conns → `Starting→Off` | `DHAN-LANE-02` (`WsPoolSpawnFailed`) | `error!` + Telegram; partial conns aborted (no orphan sockets) |
| Auth fails (token/TOTP/SSM) | auth | `Starting→Off` | `DHAN-LANE-03` (`AuthFailed`) | reuse AUTH-GAP-* semantics; `error!` + Telegram; bounded retry |
| Teardown leaves dangling tasks / WS won't close | teardown | fire `lane_shutdown` + `.abort()` each handle; await with timeout then force-abort; `Stopping→Off` only after join | `DHAN-LANE-04` (`TeardownTimeout`, Severity::Medium) | `warn!`→`error!` on timeout; `tv_dhan_lane_teardown_forced_total` |
| **Gate re-closes mid-teardown (H5)** | teardown | RE-ASSERT `can_disable_dhan()` before WS-close; abort → `Stopping→Running` | reuse the gate CONFLICT path | `warn!` "disable aborted — order opened during teardown"; `tv_dhan_lane_disable_aborted_total` |
| **Cold-start re-runs auth, would double-set global (C4)** | auth | lane uses its OWN `TokenManager`; does NOT call `set_global_token_manager` a 2nd time (or only via an FSM-guarded re-settable path) | n/a (closed by construction) | ratchet `cold_start_does_not_double_set_global_token_manager` |
| Phantom `Running` with no pool | invariant | `Running` set ONLY after `start_dhan_lane` returns `Ok(handles)` — structurally impossible without a pool; sentinel removed | n/a (closed) | old `dhan_pool_present` false-OK test rewritten "Running ⟺ handles present" |
| **Lane pool watchdog would `process::exit` (H7)** | runtime | watchdog signals FSM, never exits process | n/a (closed) | ratchet `dhan_pool_watchdog_does_not_call_process_exit` |
| Start task panics before `Running` | any | dead-activation watchdog (`is_dead_activation`) clears handle → ONE bounded re-Start (WS-GAP-05) | reuse existing | `error!` "lane start task ended without bringing the lane up" |

**Cancel-safety mechanism (load-bearing, H8):** `start_dhan_lane` is one owned
task the watcher `.abort()`s on disable. Every LANE child it spawned is tracked
in a `Vec<AbortOnDrop>` and aborted in the cancel/`Err` cleanup before return.
The PROCESS children (seal-writer, aggregator, API server, otel) are NEVER in
that Vec, so a cancel never tears them down (the §1.7 classification table is the
contract; ratchet `cancel_aborts_only_lane_handles`).

**Secret discipline (L12):** the `DHAN-LANE-0N` `error!` lines and the
`dhan_lane_audit` `reason` column carry ONLY fixed enum discriminants
(`AuthFailed` / `UniverseBuildFailed` / …), NEVER the raw Dhan auth response body
(redacted via `capture_rest_error_body`, mirroring `main.rs:1336-1338`). Ratchet
`dhan_lane_audit_has_no_secret_columns`.

---

## 4. Test Plan

Applicable categories from the 22 (`testing.md`), scoped to `crates/app`:
**unit** (FSM transitions), **integration** (lifecycle guards), **loom /
deterministic-sequence chaos** (toggle flap, teardown race), **property** (FSM
totality), the mechanical **pub-fn-test** + **pub-fn-wiring** guards for every new
pub fn; **dhat** is N/A (cold control-plane, not hot path — same honest envelope
as the lifecycle plan §244).

### D2-pre — behaviour-identical hoist
- `boot_on_behaviour_identical_after_hoist` (snapshot/log-order diff vs pre-D2-pre)
- `api_server_up_in_dhan_off_mode` (the `/api/feeds` route responds when Dhan OFF — fixes C1)
- `aggregator_and_seal_writer_run_in_dhan_off_mode` (Groww candles seal with Dhan OFF — C2)
- `dhan_lane_context_field_list_matches_compile` (L11 — the exhaustive field list compiles)

### FSM unit tests (`dhan_lane.rs` / `dhan_activation.rs`)
- `next_lane_state_off_to_starting_on_start`
- `next_lane_state_starting_to_running_on_success`
- `next_lane_state_starting_to_off_on_failure`
- `next_lane_state_starting_to_off_on_cancel`
- `next_lane_state_running_to_stopping_on_disable`
- `next_lane_state_stopping_to_off_only_after_handles_join` (H6)
- `next_lane_state_rejects_running_to_starting`
- `next_lane_state_rejects_off_to_running`
- `next_lane_state_rejects_off_to_stopping`
- `next_lane_state_rejects_stopping_to_starting` (H6)
- `next_lane_state_is_total` (property: every (state,event) returns Some|None, never panics)

### Cancel-safety / ownership tests (H8, C2)
- `start_dhan_lane_returns_cancelled_when_token_fired_before_first_await`
- `start_dhan_lane_aborts_partial_pool_conns_on_cancel`
- `cancel_aborts_only_lane_handles` (H8 — PROCESS seal-writer/aggregator/API NOT aborted)
- `start_dhan_lane_does_not_spawn_seal_writer_or_aggregator_or_api` (C2 ownership)
- `stop_dhan_lane_is_idempotent_stopping_to_off`
- `cancel_during_stopping_force_aborts_after_timeout`

### Token-safety tests (C4)
- `cold_start_does_not_double_set_global_token_manager` (C4)
- `lane_owned_token_manager_dropped_on_stop`

### Gate / teardown-race tests (H5, H6)
- `disable_aborts_if_orders_open_mid_teardown` (H5 — gate re-asserted before WS-close)
- `no_double_pool_during_teardown` (H6 — Start-during-Stopping rejected)

### Watchdog tests (H7)
- `dhan_pool_watchdog_does_not_call_process_exit` (source-scan)
- `lane_watchdog_halt_signals_fsm_not_process_exit`

### Teardown-only tests (C3)
- `stop_dhan_lane_does_not_run_partition_detach_or_shutdown_wait` (source-scan: `teardown_dhan_lane_tasks` calls neither)

### Fast-arm tests (L10 / D2d)
- `runtime_toggle_never_enters_fast_arm`

### Toggle-flap / chaos (extend `feed_toggle_lifecycle_guard.rs`)
- `dhan_cold_start_storm_converges_to_final_on`
- `dhan_cold_start_storm_no_double_start`
- `dhan_cold_start_off_at_boot_then_on_reaches_running`
- `dhan_cold_start_cancel_mid_starting_returns_to_off`
- loom (if cheap) `dhan_lane_fsm_no_lost_transition_under_interleaving`

### Boot-isolation (extend `per_feed_boot_isolation_guard.rs`)
- `off_dhan_feed_does_zero_work_until_first_on`
- `dhan_off_boot_no_longer_early_returns_before_watcher_has_context` (source-scan)

### Observability / secret discipline (L12)
- `dhan_lane_audit_has_no_secret_columns`
- `tv_dhan_lane_state_gauge_set_on_transition_only` (edge-triggered)

### Wiring/mechanical
- `pub-fn-wiring-guard.sh`: `start_dhan_lane` call site = inline boot (D2a) + watcher (D2b); `stop_dhan_lane` call site = watcher disable + graceful shutdown; `teardown_dhan_lane_tasks` call site = `stop_dhan_lane`.
- `pub-fn-test-guard.sh`: every new pub fn has a `#[test]` or `// TEST-EXEMPT:`.
- `error_code_rule_file_crossref.rs`: every new `DHAN-LANE-0N` variant in a rule file (`.claude/rules/project/dhan-lane-error-codes.md`).
- `error_code_tag_guard.rs`: every `error!` carries `code = ErrorCode::X.code_str()`.

### Per-PR commands
`cargo test -p tickvault-app --lib --tests`, `cargo test -p tickvault-api --lib`
(feed_state), `cargo clippy --workspace -- -D warnings -W clippy::perf`,
`cargo fmt --check`, the 4 hooks above, 3-agent adversarial review on EACH PR.

---

## 5. Rollback

D2-pre is a pure relocation with NO new runtime path — it is `git revert`-able to
the inline-block layout; its only risk is the boot refactor, fully covered by
`boot_on_behaviour_identical_after_hoist` + full `cargo test`.

A config feature flag `feeds.dhan_runtime_cold_start` gates the NEW runtime-start
path:
- **flag false** (D2-pre, D2a) → the Dhan-OFF boot behaves as today: no
  cold-start (the watcher records the desired flag but keeps `LaneState::Off`).
  Boot-ON is byte-identical regardless of the flag (it calls `start_dhan_lane`
  inline, which IS the old spine). After D2-pre the API server/aggregator/
  seal-writer ARE up in Dhan-OFF mode (the intended, tested change), but no Dhan
  lane starts.
- **flag true** (D2b) → the watcher drives `Off→Starting→Running` on a runtime
  enable.

The flip-to-off is a TESTED path:
`dhan_cold_start_flag_off_falls_back_to_boot_time_only` asserts that with the flag
false a runtime Dhan enable on a boot-OFF run records the desired flag but keeps
`LaneState::Off`. Each sub-PR is independently `git revert`-able: D2-pre →
inline-block layout; D2a → inline spine; D2b → "watcher refuses cold-start"; D2c →
guards; D2d → split fast/slow arms.

---

## 6. Observability

7-layer telemetry for EACH new lifecycle transition (edge-triggered, Rule 4;
market-hours-aware where the transition implies live data, Rule 3):

| Layer | Artefact |
|---|---|
| 1. Counter | `tv_dhan_lane_transitions_total{from,to}` (edge — one per transition); `tv_dhan_lane_start_failed_total{stage}`; `tv_dhan_lane_teardown_forced_total`; `tv_dhan_lane_disable_aborted_total` (H5) |
| 2. Gauge | `tv_dhan_lane_state` (0=Off,1=Starting,2=Running,3=Stopping) — **set ONLY on transition (edge), never per-poll (L12)** |
| 3. Tracing span | `#[instrument]` on `start_dhan_lane` / `stop_dhan_lane` / `teardown_dhan_lane_tasks` (cold path) |
| 4. `error!` w/ code | `DHAN-LANE-01..04` with `code = ErrorCode::X.code_str()` (tag-guard); `reason` = fixed enum discriminant, never raw auth body (L12) |
| 5. Telegram | new `NotificationEvent::DhanLaneStarted` (Info) / `DhanLaneStartFailed { stage, reason }` (High) / `DhanLaneStopped` (Info) / `DhanLaneDisableAborted` (Low, H5) — auto-driver wording, Severity-routed |
| 6. Audit row | `dhan_lane_audit` QuestDB table, DEDUP `(trading_date_ist, transition_kind, ts)`; `reason` column carries ONLY enum discriminants — ratchet `dhan_lane_audit_has_no_secret_columns` (L12) |

Edge-trigger: a transition fires its counter/Telegram/gauge ONCE on the edge. The
`Starting→Running` success emits the positive signal (no false-OK — only after
`start_dhan_lane` returns Ok). `tv_dhan_lane_state` gives the operator a real-time
"Off/Starting/Running/Stopping" without inferring from logs. Retry backoff for a
failed start is pinned to Groww's cap `min(10 * 2^n, 300)` seconds (L12).

---

## Per-Item Guarantee Matrix (15-row + 7-row)

This plan and EVERY sub-PR (D2-pre/D2a/D2b/D2c/D2d) carry the full 15-row "100%
everything" matrix and the 7-row Resilience Demand matrix — cross-referenced from
`.claude/rules/project/per-wave-guarantee-matrix.md` (all 15 + all 7 rows apply
verbatim). Highlights specific to D2:

- **Audit coverage:** new `dhan_lane_audit` table (Layer 6).
- **Code performance / O(1):** N/A axis — cold control-plane; the hot tick path
  (already O(1)) is UNTOUCHED. `start_dhan_lane` is a boot-equivalent cold path.
- **Monitoring/alerting:** the 7-layer telemetry (§6) + a CloudWatch alarm on
  `tv_dhan_lane_start_failed_total > 0`.
- **Bugs/code review:** 3-agent adversarial review (hot-path + security + hostile)
  on EACH sub-PR diff, before AND after impl (charter §E) — this revision IS the
  fold-in of one such pass (§0.5).
- **Security:** the dry_run-only disable gate (`can_disable_dhan`) is re-asserted
  across teardown (H5); no token in logs/audit (L12 secret discipline); the lane
  owns its `TokenManager` so a cold-start can't silently renew a stale token (C4).
- **Resilience (7-row):** zero-tick-loss WAL/ring/spill/DLQ reused unchanged;
  `SubscribeRxGuard` reused unchanged; composite-key dedup unaffected; 2-WS lock
  held; PROCESS-shared aggregator/seal-writer survive Dhan OFF (Groww unaffected);
  real-time proof via §6 telemetry + the lifecycle guards.

---

## Honest envelope (mandatory wording, charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: D2 makes
> a boot-OFF Dhan feed cold-startable at runtime. The previous 'pure extraction'
> premise was FALSE — the API server, tick broadcast, 21-TF aggregator +
> seal-writer, and the main run-loop all lived INSIDE the Dhan block — so D2 FIRST
> hoists that PROCESS-shared infra out (D2-pre, behaviour-identical) so the
> `/api/feeds` endpoint + Groww candle-sealing survive Dhan OFF, THEN extracts the
> Dhan spine into a callable `start_dhan_lane()` driven by the existing dormant
> watcher (D2a/D2b). It reuses the SAME WAL→ring→spill→DLQ + `SubscribeRxGuard`
> zero-tick-loss machinery, adds NO WebSocket endpoint (the 2-Dhan-WS lock holds),
> preserves the dry_run-only `can_disable_dhan` gate AND re-asserts it across
> teardown (no blinding mid-trade), and gives the lane its OWN `TokenManager` so a
> cold-start can't silently renew a stale token. The lane pool watchdog is
> lane-scoped and never `process::exit`s, so a lane Halt can't kill Groww. A
> failed cold-start returns the lane to `Off` with a typed ErrorCode + Telegram —
> NEVER a half-running lane, NEVER tearing down PROCESS infra. The FSM
> (`Off→Starting→Running→Stopping→Off`, with `Stopping→Off` only after all handles
> join and `Stopping→Starting` rejected) is pure-unit-tested for totality; the
> teardown-race, cancel-safety, and toggle-storm cases are ratcheted by
> deterministic guards. Boot-ON is byte-identical. NOT claimed: that the fast-boot
> crash-recovery arm is folded in (tracked as D2d, with a guard that a runtime
> toggle can never enter it), nor that every Dhan-side failure self-heals — the
> retry envelope is bounded (`min(10*2^n, 300)`s), beyond it the lane stays `Off`
> and the operator is paged."

---

## Implementation sub-PR breakdown (serial, each independently reviewable)

| Sub-PR | Scope | Risk | Behaviour change |
|---|---|---|---|
| **D2-pre** | **Shared-infra HOIST.** Lift the API server (+ `FeedRuntimeState`/`/api/feeds` routes), `tick_broadcast_sender`, the 21-TF aggregator + seal-writer (`global_seal_sender`), and the main run-loop OUT of the Dhan block into PROCESS-shared infra that runs regardless of Dhan ON/OFF. Split `run_shutdown_fast` into the PROCESS run-loop + a real graceful-shutdown teardown. First task: enumerate the exhaustive `DhanLaneContext` field list from a real compile (L11). | Medium (boot refactor) but **behaviour-identical** | The PROCESS infra now runs in Dhan-OFF mode (API server up → `/api/feeds` reachable, Groww candles seal). Boot-ON byte-identical. No runtime cold-start yet. |
| **D2a** | **Pure extraction** of the now-isolated slow-arm Dhan spine into `crates/app/src/dhan_lane.rs::start_dhan_lane` + `DhanLaneContext`/`DhanLaneHandles` + `teardown_dhan_lane_tasks`. The boot path calls `start_dhan_lane` inline EXACTLY where the spine ran; the new graceful-shutdown teardown calls `teardown_dhan_lane_tasks`. Lane owns its `TokenManager` (C4) + lane-scoped pool watchdog (H7). Flag `feeds.dhan_runtime_cold_start` added, default **false**. | Medium (SEBI-spine refactor) but **behaviour-identical** | NONE — boot-ON byte-identical; boot-OFF still idles (no cold-start path yet). Verified by snapshot/diff + full `cargo test`. |
| **D2b** | **Runtime path + FSM.** Add `LaneState` as an atomic in `FeedRuntimeState` (M9) + `next_lane_state`; give the dormant watcher (`main.rs:377`) the `DhanLaneContext`; on `Off`+desired-ON drive `Off→Starting`→`start_dhan_lane`→`Running`; on gated disable drive `Running→Stopping`→`stop_dhan_lane` (which re-asserts the gate, H5, and sets `Stopping→Off` only after handles join, H6)→`Off`. Change the Dhan-OFF boot from early-return to "build ctx → hand to watcher → fall into shared run-loop". Lane-scoped watchdog shares the lane Notify (H7). Remove `start_marks_running` "no pool" residual + `dhan_pool_present` sentinel. Flip flag default **true**. | High (the actual new behaviour) | NEW: boot-OFF Dhan is now runtime-startable. Gated by the flag for rollback. |
| **D2c** | **Tests + observability.** All §4 FSM/cancel/ownership/token/gate/watchdog/teardown/chaos/wiring tests; the §6 7-layer telemetry (counters, edge-triggered `tv_dhan_lane_state` gauge, spans, `DHAN-LANE-01..04` ErrorCodes + `dhan-lane-error-codes.md` rule file, Telegram events, `dhan_lane_audit` table with secret-free `reason`); CloudWatch alarm. Strengthen `per_feed_boot_isolation_guard.rs` + `feed_toggle_lifecycle_guard.rs`. | Low (additive) | NONE (observability + guards only). |
| **D2d** | **Fast-arm unification (follow-up).** Fold the fast crash-recovery boot arm (`:1211-2055`) into `start_dhan_lane` so both arms share one code path. Keep/strengthen the guard `runtime_toggle_never_enters_fast_arm`. | Medium (boot refactor) | NONE intended — fast arm becomes a cache-warmed entry into the same lane fn; verified byte-identical. |

Serial per `pr-completion-protocol.md`: D2-pre → main; D2a rebases → main; D2b
rebases; D2c rebases; D2d rebases. Each carries the 15+7 matrix, the honest
envelope, and the 3-agent review.
