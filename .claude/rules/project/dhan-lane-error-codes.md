# Runtime Dhan-Lane Cold-Start FSM — Error Codes (DHAN-LANE-01..04)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `websocket-connection-scope-lock.md` (the 2-Dhan-WS lock + PR-E runtime-toggle
> + dry_run-only disable gate) > this file.
> **Operator authorization:** PR-E (2026-06-21, `websocket-connection-scope-lock.md`)
> — *"if I want to switch off or on dhan also it should be accepted right dude"*;
> the AskUserQuestion **"Fully disconnect Dhan"** = OFF closes the Dhan WS(es) +
> stops storing; ON reconnects + re-subscribes. D2b is the runtime cold-start
> half of that lifecycle for a Dhan feed that was **OFF at boot**.
> **Companion design:** `.claude/plans/active-plan-dhan-cold-start-d2.md` (the
> §0.5 C1–C8 / H5–H8 safety-fix contract).
> **Companion code:** `crates/app/src/main.rs`
> (`LaneState`/`next_lane_state` in `crates/api/src/feed_state.rs`,
> `start_dhan_lane` / `stop_dhan_lane` / `teardown_dhan_lane_tasks` /
> `run_dhan_lane_runtime` runtime wrapper), `crates/app/src/dhan_activation.rs`
> (the level-triggered watcher that drives the FSM).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `DhanLane*` variant verbatim.

---

> **⚠ RETIREMENT AUTHORIZED 2026-07-13 (Dhan live WS retired — deletion lands with the
> Phase C PRs):** the runtime Dhan-lane cold-start FSM this file documents is retired with
> the Dhan live WS (operator directive 2026-07-13, verbatim in
> `websocket-connection-scope-lock.md` "2026-07-13 Amendment"). Phase A already refuses the
> runtime cold-start (raw-TOML gate + the /feeds 409); the Phase C code PRs DELETE the lane
> (`LaneState` / `start_dhan_lane` / `stop_dhan_lane` / `run_dhan_lane_runtime`) and with it
> ALL FOUR codes — `DHAN-LANE-01` (`DhanLane01UniverseBuildFailed`; its universe-build
> emit site dies with the instrument chain per Q3), `DHAN-LANE-02`
> (`DhanLane02WsPoolSpawnFailed`), `DHAN-LANE-03` (`DhanLane03AuthFailed` — auth-failure
> visibility for the retained Dhan surface lives on in the `dhan_rest_stack` backoff arms
> + the existing AUTH-GAP-* codes; no replacement variant), and `DHAN-LANE-04`
> (`DhanLane04TeardownTimeout`). The order-update WS the lane used to spawn is REWIRED
> into `dhan_rest_stack` (functional-dormant, operator Q4-i "agreed dude"). Content below
> retained for historical audit.

## §0. Why these codes exist (the locked design)

A Dhan feed that was **OFF at boot** has no main-feed WS pool. PR-E's in-loop
dormancy can RESUME a pool that exists, but it cannot CREATE one. D2b closes
that gap with a runtime cold-start FSM driven by the SAME dormant watcher the
Groww feed uses (`groww_activation.rs` is the precedent: one owned abortable
`JoinHandle` + `Arc<Notify>`).

The lane FSM (`LaneState`, an `AtomicU8` in `FeedRuntimeState`):

```
 Off ──desired ON──▶ Starting ──Ok──▶ Running ──gated OFF──▶ Stopping ──join──▶ Off
   ▲                    │  start fails / cancelled              │  gate re-closed
   └────────────────────┘─────────────────────────────────────┘  (back to Running)
```

`Running` is entered **ONLY** after `start_dhan_lane` returns `Ok` — closing
the "phantom running" false-OK. `Stopping → Off` happens **ONLY** after the
teardown awaits every WS/order-update/renewal handle join (no double pool).
`Stopping → Starting` is **rejected** by the FSM. The FSM is the pure-total
`next_lane_state(state, event) -> Option<LaneState>` (`None` = illegal).

**Safety invariants (operator-locked):**
- The lane OWNS its `TokenManager` end-to-end (dropped on stop) — a cold-start
  cannot silently renew a stale global token (C4).
- The Dhan pool watchdog is **lane-scoped**, shares the lane `Arc<Notify>`, and
  on its Halt path signals the FSM — it MUST NOT `std::process::exit` (H7), so a
  lane Halt can never kill the process / the Groww feed.
- `stop_dhan_lane` RE-ASSERTS `can_disable_dhan()` immediately before the
  irreversible WS-close (H5); if the gate re-closed (an order opened
  mid-teardown), teardown ABORTS and the FSM returns to `Running` — the system
  can never be blinded mid-trade.
- A partial start aborts EVERY lane-owned handle spawned so far (cancel-safety,
  H8); the PROCESS-shared seal-writer / 21-TF aggregator / API server are NEVER
  created or torn down by the lane.

**Observability (D2b core):** `tv_dhan_lane_state` gauge (0=Off, 1=Starting,
2=Running, 3=Stopping) set ONLY on transition (edge-triggered);
`tv_dhan_lane_transitions_total{from,to}`; `tv_dhan_lane_start_failed_total{stage}`;
`tv_dhan_lane_teardown_forced_total`; `tv_dhan_lane_disable_aborted_total`. The
full `dhan_lane_audit` QuestDB table + typed Telegram `NotificationEvent`
variants are deferred to **D2c**.

Backoff between failed cold-start attempts is Groww's cap `min(10 * 2^n, 300)`s.

---

## §1. DHAN-LANE-01 — universe-build stage failed

**Severity:** High. **Auto-triage safe:** Yes (bounded retry re-attempts).

**Trigger:** `start_dhan_lane` returned its daily-universe-build boot-abort at
runtime (the inline spine's instrument-load failure). The lane FSM is driven
`Starting → Off`; a bounded backoff retry is scheduled. No pool was spawned, so
no tick is lost.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `DHAN-LANE-01`; the `reason` is a
   FIXED enum discriminant (`universe_build_failed`), never a raw body.
2. Cross-check `INSTR-FETCH-*` (the universe fetch chain's own typed codes) —
   the underlying cause is usually a CSV-fetch / parse / dangling-ref failure.
3. The next backoff window re-attempts automatically. Sustained failure means
   the upstream Dhan CSV / niftyindices source is degraded — see those runbooks.

**Source:** `crates/common/src/error_code.rs::ErrorCode::DhanLane01UniverseBuildFailed`,
`crates/app/src/main.rs` (`run_dhan_lane_runtime` failure classify + emit).

---

## §2. DHAN-LANE-02 — main-feed WS-pool create/spawn stage failed

**Severity:** High. **Auto-triage safe:** Yes (bounded retry; partial work aborted).

**Trigger:** `start_dhan_lane` returned its WS-pool create/spawn boot-abort at
runtime. The cancel cleanup aborted EVERY lane-owned handle spawned so far (no
orphan sockets, no leaked tasks), and the FSM is driven `Starting → Off`. The
2-Dhan-WS lock is untouched (no new endpoint, no 2nd main-feed conn).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `DHAN-LANE-02` (`reason =
   ws_pool_spawn_failed`).
2. Cross-check `tv_websocket_connections_active` and the WS read-loop logs —
   a TLS / DNS / Dhan-side handshake failure is the usual cause.
3. The next backoff window re-attempts; the PROCESS-shared infra (API server,
   aggregator, seal-writer) stays up throughout, so the Groww feed is unaffected.

**Source:** `crates/common/src/error_code.rs::ErrorCode::DhanLane02WsPoolSpawnFailed`,
`crates/app/src/main.rs` (`run_dhan_lane_runtime`).

---

## §3. DHAN-LANE-03 — auth / IP / static-IP / dual-instance boot gate failed

**Severity:** High. **Auto-triage safe:** Yes (bounded retry).

**Trigger:** `start_dhan_lane` returned its boot-gate abort at runtime — IP
verify, TOTP→JWT auth, the Dhan static-IP gate, or the dual-instance lock. At
RUNTIME this NEVER exits the process (unlike the boot path's clean exit); the
FSM returns `Starting → Off` and the operator is paged; bounded retry follows.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `DHAN-LANE-03` (`reason =
   auth_or_gate_failed`).
2. Reuse AUTH-GAP-* semantics: check `tv_token_remaining_seconds`, the SSM
   TOTP-secret, and the static-IP whitelist (`GET /v2/ip/getIP`).
3. The next backoff window re-attempts. The lane owns its own `TokenManager`
   (C4), so a runtime cold-start can never renew a stale global token.

**Source:** `crates/common/src/error_code.rs::ErrorCode::DhanLane03AuthFailed`,
`crates/app/src/main.rs` (`run_dhan_lane_runtime`).

---

## §4. DHAN-LANE-04 — teardown drain timeout (lane still reached Off)

**Severity:** Medium. **Auto-triage safe:** Yes (degraded-but-recovered).

**Trigger:** `stop_dhan_lane` → `teardown_dhan_lane_tasks` hit the bounded
pool-supervisor drain timeout and force-aborted the remaining WS handles. The
lane STILL reaches `LaneState::Off` (handles joined-or-force-aborted), so this
is degraded-but-recovered, not data loss. `tv_dhan_lane_teardown_forced_total`
increments.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `DHAN-LANE-04`.
2. A hung WS handle at teardown usually means a slow/black-holed socket close.
   The lane is already `Off`; no operator action is required beyond noting it.
3. If it recurs every disable, inspect the WS graceful-close path
   (`request_graceful_shutdown` / RequestCode 12).

**Honest envelope:** the teardown is bounded — it never blocks forever. A drain
timeout force-aborts and proceeds to `Off`; the only loss is the in-flight
graceful-close of an already-disabled feed.

**Source:** `crates/common/src/error_code.rs::ErrorCode::DhanLane04TeardownTimeout`,
`crates/app/src/main.rs` (`teardown_dhan_lane_tasks`).

---

## §5. What a PR that violates this lock looks like (REJECT)

- Enters `LaneState::Running` before `start_dhan_lane` returns `Ok` (phantom running).
- Sets `Stopping → Off` before the teardown awaits all handles join (double-pool risk).
- Adds a legal `Stopping → Starting` transition to `next_lane_state` (double pool).
- Makes the Dhan pool watchdog call `std::process::exit` on its Halt path (H7 —
  would kill the process + the Groww feed on a lane-only Halt).
- Removes the `can_disable_dhan()` re-assert in `stop_dhan_lane` before the
  irreversible WS-close (H5 — could blind the feed mid-trade).
- Has `start_dhan_lane` create or `stop_dhan_lane` tear down the PROCESS-shared
  seal-writer / 21-TF aggregator / API server / otel (C2 — would kill Groww).
- Relies on the global token `OnceLock` for the lane's working token instead of
  the lane-owned `TokenManager` (C4).
- Adds a new Dhan WS endpoint or a 2nd main-feed connection (2-WS lock).

---

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `DhanLane*` variant)
- `crates/api/src/feed_state.rs` (`LaneState` / `next_lane_state`)
- `crates/app/src/dhan_activation.rs`
- `crates/app/src/main.rs` (`start_dhan_lane` / `stop_dhan_lane` /
  `teardown_dhan_lane_tasks` / `run_dhan_lane_runtime`)
- Any file containing `DHAN-LANE-`, `DhanLane0`, `LaneState`, `next_lane_state`,
  `tv_dhan_lane_state`, or `run_dhan_lane_runtime`
