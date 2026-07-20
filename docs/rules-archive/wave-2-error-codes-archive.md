# Archived sections — wave-2-error-codes.md

> Sections excised verbatim from `.claude/rules/project/wave-2-error-codes.md`
> on 2026-07-20 (context-size incident). Live file keeps a pointer per section.


<!-- ==== WS-GAP-05, WS-GAP-06, WS-GAP-07, WS-GAP-08, WS-GAP-09 (variants RETIRED in the C4 sweep 2026-07-15 — Dhan live-WS lane deleted; crossref-allowlisted; retained for historical audit) ==== -->

## WS-GAP-05 — pool slot respawned after an unexpected clean exit

> **⚠ VARIANT RETIRED in the C4 sweep (2026-07-15):** the `WsGap05PoolRespawn` ErrorCode
> variant is DELETED — its only emit sites (the Dhan main-feed pool supervisor slot loop) were
> deleted in Phases C2/C3 (#1522/#1569) with the Dhan live WS (operator
> 2026-07-13, `websocket-connection-scope-lock.md` "2026-07-13 Amendment").
> Content below retained for historical audit; the code string stays
> reverse-crossref-allowlisted.

> **2026-07-10 truth-sync (W2#8).** The pre-2026-07-10 text here described a
> `respawn_dead_connections_loop` / `spawn_pool_supervisor_task` design that
> was NEVER BUILT (neither function ever existed; the shipped
> `supervise_pool` was a log-and-count teardown drain whose own doc-comment
> said "actual respawn is deferred"). W2#8 implements the respawn with
> different — safer — semantics, described below.

**Trigger (actual):** the per-slot task spawned by
`WebSocketConnectionPool::spawn_all` runs the supervised loop
`run_supervised_pool_slot` (`connection_pool.rs`). When `conn.run()`
exits `Ok(())` WITHOUT a shutdown request and with a live downstream
frame channel — i.e. a Dhan **server Close frame** or a **TCP
stream-end** — the loop logs `error!(code = "WS-GAP-05")`, increments
`tv_ws_pool_respawn_total{reason="unexpected_clean_exit"}`, sleeps a
storm-bounded backoff (5s base per the original contract, doubling to a
300s cap; a session that survives ≥60s resets the ladder — the WS-GAP-10
stability-window precedent), and re-enters `run()` **in the SAME task on
the SAME slot** — replace-not-add, so the 2-WebSocket lock and the lane
handle ownership (`DhanLaneRunHandles` H8 Drop floor,
`teardown_dhan_lane_tasks` abort+drain) are structurally unchanged.
Pre-W2#8 this exit class left the slot dead until the pool watchdog's
300s Halt + 15-min WS-GAP-09 ride-out (which assumed a live reconnect
loop and therefore no-op'd on the dead task) escalated to a full
process restart — ~5–20 min of dead feed for a 5-second problem.

**What NEVER respawns (deliberate exits stay terminal):** graceful
shutdown / lane teardown (`is_shutdown_requested`), a teardown `abort()`
(cancels the whole loop — a torn-down lane can never be resurrected),
`NonReconnectableDisconnect` (the Dhan 805-class anti-storm stop),
`ReconnectionExhausted` (a configured finite retry budget), and a clean
exit with the frame channel CLOSED (WS-GAP-07 — the tick consumer is
dead; respawning would churn Dhan connects with zero benefit).

**Honest panic envelope (the TICK-FLUSH-01 precedent):** the workspace
release profile sets `panic = "abort"`, so in the PRODUCTION binary a
panicked connection task ABORTS the process — "respawn on panic" was
always physically impossible in release and is NOT claimed; recovery for
a panic is process restart + WAL replay. In unwind (dev/test) builds a
panic propagates out of the slot task and surfaces at the teardown
drain, exactly as before.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `WS-GAP-05`; the payload
   carries `connection_id`, `backoff_secs`, `consecutive_quick_exits`.
2. A one-off respawn that recovers (ticks resume) = healthy self-heal,
   no action. Repeated respawns
   (`tv_ws_pool_respawn_total{reason="unexpected_clean_exit"}` rate
   sustained) mean Dhan keeps closing our session server-side —
   cross-check the token (`tv_token_remaining_seconds`, AUTH-GAP-*),
   Dhan status, and the connection count (max 5 WS/account); the loop
   is already backing off to the 300s ceiling — loud, never silent.
3. The pre-existing teardown-drain labels (`ws_error` / `panic` /
   `cancelled` / `unknown`) on the same counter fire at lane teardown
   when the drain observes how each handle resolved — every deliberate
   teardown reports `cancelled` there (the handles are aborted first);
   that is shutdown bookkeeping, not a live respawn.

**Honest envelope (2026-07-10 adversarial-review outcomes):**
- **Off-hours parity (MEDIUM, FIXED):** a clean-close exit never increments
  `run()`'s reconnect counter, so the WS-GAP-04 post-close sleep could
  never engage for this class — an off-hours Close-frame loop would have
  churned connects all night. The Respawn arm therefore re-runs the SAME
  market-hours gate the initial spawn uses (no-op in market hours,
  park-until-09:00-IST off-hours) and re-checks the shutdown flag after
  the backoff sleep AND after the gate, so a graceful shutdown landing
  mid-sleep/mid-park can never open a fresh socket (the A5 notify permit
  + teardown abort remain the hard floor).
- **60s-metronome residual (MEDIUM, ACCEPTED — the WS-GAP-10 precedent):**
  a server that consistently closes each session at ≥61s resets the
  stability streak every cycle and pins the backoff at the 5s base —
  worst case ~1 reconnect per ~66s on the locked 1-connection pool
  (~1,300/day, inside Dhan limits), each cycle loud (one coded ERROR +
  counter). Strictly better than the pre-W2#8 behaviour for the same
  pattern (dead slot → 300s Halt → process-restart loop).
- **Audit asymmetry (LOW, flagged follow-up):** the clean-close exit path
  in `run()` stamps no `Disconnected` audit row / Telegram, and a
  respawned session's first connect stamps "initial connect" (the
  reconnect counter is fresh) — the WS-GAP-05 ERROR + counter are the
  operator signal for this class today. Wiring a typed audit row into
  the respawn arm is a follow-up (would touch `run()`'s audit surface,
  deliberately out of this PR's no-state-machine-change scope).

**Ratchet:**
`connection_pool.rs::tests::ratchet_spawn_all_uses_supervised_slot_loop`
(pins the spawn wiring, classify call, counter, code field, backoff,
market-hours gate, and the double shutdown re-check; + the
classify/backoff/terminal-behaviour unit + tokio tests beside it).

**Source:** `crates/core/src/websocket/connection_pool.rs::{run_supervised_pool_slot, classify_pool_slot_exit, compute_pool_respawn_backoff_secs}`; teardown drain: `WebSocketConnectionPool::supervise_pool`.

## WS-GAP-06 — tick-gap detector fired a coalesced summary

> **⚠ RETIREMENT AUTHORIZED 2026-07-13 — deletion LANDED in PR-C3 (2026-07-14):** the
> tick-gap detector and this code are DELETED with the Dhan live WS (operator Q4-ii
> "agreed dude" — *tick-gap detector + WS-GAP-06 deleted; the Groww feed-stall watchdog
> owns stall detection*, 2026-07-13; full contract in `websocket-connection-scope-lock.md`
> "2026-07-13 Amendment"). The detector was fed ONLY by the Dhan WS pipeline
> (`record_tick_global` at `tick_processor.rs`; the Groww bridge never recorded into it —
> Phase B map, Verified), so post-retirement it is a no-input shell. Honest envelope of
> the deletion: FEED-level stall detection for Groww is `FEED-STALL-01`
> (`feed-stall-watchdog-error-codes.md`); PER-SID silence visibility moves to the
> scoreboard presence/coverage columns (15:45 IST cadence) — there is deliberately NO 30s
> per-SID page anymore. The `tv_tick_gap_instruments_silent` gauge, the WS-GAP-06 seeding,
> and the §36 far-month alarm-gate exclusion die with it. Content below retained for
> historical audit. **C4 sweep (2026-07-15): the `WsGap06TickGapSummary` variant is now
> DELETED** (retained through C3 only to keep the crossref green; the reverse check
> allowlists the code string).

**Trigger:** Item 8's `TickGapDetector` observed ≥1 instrument with
silence ≥30s during the most recent 60s coalesce window.

**Triage:**
1. Inspect `data/logs/errors.jsonl.*` for the `TickGapsSummary` event;
   it carries a `top_10_samples` list `(symbol, gap_secs)`.
2. If gap symbols cluster on one segment, check for a Dhan-side
   ExchangeSegment outage.
3. If gap symbols are scattered, check `tv_websocket_connections_active`
   — a slow socket can starve a subset of instruments.

**Source:** `crates/core/src/pipeline/tick_gap_detector.rs::TickGapDetector::scan`

## WS-GAP-07 — live-feed frame channel closed (tick consumer died)

> **⚠ VARIANT RETIRED in the C4 sweep (2026-07-15):** the `WsGap07LiveChannelClosed` ErrorCode
> variant is DELETED — its only emit sites (the Dhan main-feed read loop's frame-channel Closed arm) were
> deleted in Phases C2/C3 (#1522/#1569) with the Dhan live WS (operator
> 2026-07-13, `websocket-connection-scope-lock.md` "2026-07-13 Amendment").
> Content below retained for historical audit; the code string stays
> reverse-crossref-allowlisted.

**Trigger:** the main-feed read loop's `frame_sender.try_send(frame)`
returned `TrySendError::Closed` — the downstream tick-processing consumer
(`run_tick_processor`) holding the `Receiver` has been dropped. The read
loop logs `error!` (code `WS-GAP-07`), increments
`tv_ws_live_channel_closed_drop_total{ws_type="live_feed"}`, and returns,
stopping frame forwarding on that connection. Severity::High.

**Why High (not Low like the post-close sleep):** unlike a backpressure
`Full` (the WAL still durably records the frame), a `Closed` channel means
the consumer task is GONE — no ticks reach the pipeline from this
connection until the consumer/app restarts. The previous code logged this
at `warn!` with no counter, so the operator was blind to a dead consumer
mid-market (audit Rule 5 — drain failures must be `error!`).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — search for `WS-GAP-07`; the event
   carries `connection_id`.
2. Check `tv_ws_live_channel_closed_drop_total` rate — a non-zero value
   during market hours means the tick processor died. Look in
   `data/logs/errors.jsonl.*` for a panic backtrace from
   `run_tick_processor` immediately preceding the WS-GAP-07 line.
3. The pool supervisor (WS-GAP-05) respawns the *connection* task, but it
   cannot resurrect a dead *consumer*. If the consumer is gone, restart the
   app — boot re-creates the channel + consumer.
4. 2026-07-06: WS-GAP-07 now pages via its `tv-<env>-errcode-ws-gap-07`
   log-filter alarm (`deploy/aws/terraform/error-code-alarms.tf`); the 5-sink
   "error!→Telegram" premise was stale post-CloudWatch-migration.

**Source:** `crates/core/src/websocket/connection.rs` (the
`TrySendError::Closed` arm of the live-feed `try_send`).

## WS-GAP-08 — Dhan 429 rate-limit cooldown persisted across a process restart

> **⚠ VARIANT RETIRED in the C4 sweep (2026-07-15):** the `WsGap08RateLimitCooldown` ErrorCode
> variant is DELETED — its only emit sites (the Dhan main-feed 429 classification + boot-wait sites) were
> deleted in Phases C2/C3 (#1522/#1569) with the Dhan live WS (operator
> 2026-07-13, `websocket-connection-scope-lock.md` "2026-07-13 Amendment").
> Content below retained for historical audit; the code string stays
> reverse-crossref-allowlisted.

**Trigger:** the main-feed WebSocket connect was rate-limited by Dhan with
HTTP 429 (DATA-805 "too many requests/connections" class). The in-memory
`rate_limit_streak` 60s→120s→240s→300s reconnect floor already absorbs this
WITHIN a running process — but when every connection has been down for
`POOL_HALT_SECS` (300s) the pool watchdog returns Halt and the BOOT-ON path
calls `std::process::exit(2)` so a supervisor restarts the process. That
restart wipes the in-memory streak to `0`, and the fresh process reconnects
with a `0ms` first retry (`compute_reconnect_base_delay_ms(0) == 0`) straight
back into Dhan's still-active 429 window → instant 429 → 300s → exit →
restart → **infinite loop**.

This code tags two events from the persisted-cooldown fix
(`crates/core/src/websocket/rate_limit_cooldown.rs`):

1. **Boot-time wait (the common, healthy case)** — at boot, BEFORE the first
   Dhan WS connect, the app reads the persisted cooldown
   (`./data/ws-rate-limit-cooldown.json`, sibling of the WS-frame WAL). If a
   cooldown is still active, the app `info!`s with `code = WS-GAP-08`,
   increments `tv_ws_rate_limit_cooldown_waited_total`, and
   `tokio::time::sleep`s out the remaining time (capped at
   `WS_RATE_LIMIT_BACKOFF_CAP_MS` = 300s) so it does NOT reconnect into the
   still-active 429 window. This is the fix that breaks the loop.
2. **Persist-write failure (rare)** — `record_rate_limit_hit` could not write
   the advisory file (disk full / read-only). Logged at `error!` with
   `code = WS-GAP-08`. Best-effort only: the in-memory streak still applies
   for the running process; only the cross-restart protection is lost until
   the disk recovers.

**Severity:** Low. The cooldown is an advisory, **fail-open** protection: a
missing / corrupt / stale file NEVER blocks boot, and the wait is bounded by
the cap so a bad file can never hang boot beyond 5 minutes. It does NOT
change the 429 backoff math (that is correct) and does NOT touch the
reconnect engine.

**Triage:**
1. Seeing the boot-time wait `info!` (event 1) is the system working as
   intended — it is waiting out a real Dhan rate-limit before reconnecting.
   If it recurs every boot, Dhan is sustainedly rate-limiting this account:
   check `tv_ws_rate_limit_cooldown_waited_total` rate and the connection
   count (max 5 WS/account); cross-check DATA-805 in
   `mcp__tickvault-logs__tail_errors`.
2. An `error!` (event 2) means the advisory file could not be written:
   `df -h data/` + `ls -la data/` for disk-full / permission / mount issues.
   The running process is still protected by its in-memory streak; the gap
   is only across a restart until the disk is fixed.

**Auto-triage safe:** YES (Severity::Low; the wait is self-bounded and the
file is fail-open).

**Source:** `crates/core/src/websocket/rate_limit_cooldown.rs`
(`remaining_cooldown_ms` / `read_cooldown` / `record_rate_limit_hit`),
the 429 classification write site in
`crates/core/src/websocket/connection.rs`, the boot read+wait sites in
`crates/app/src/main.rs` (`start_dhan_lane` slow lane + the FAST
crash-recovery boot arm — see the 2026-07-06 update below),
`crates/common/src/error_code.rs::WsGap08RateLimitCooldown`.

### 2026-07-06 Update — FAST crash-recovery boot arm now waits too (audit gap closed)

**The gap (audit-confirmed HIGH, 2026-07-06):** the boot-time wait above
(`wait_out_persisted_ws_rate_limit_cooldown`) had exactly ONE call site —
the SLOW lane (`start_dhan_lane`, gated on `should_connect_ws`). The FAST
crash-recovery boot arm (market-hours restart with a valid cached token)
created + spawned its WS pool with NO cooldown read. A mid-market
`process::exit(2)` — still reachable via the WS-GAP-09 `ceiling_exceeded`
fallback — with a valid cached JWT (a 429 does NOT invalidate the token)
routes through FAST BOOT, wipes the in-memory `rate_limit_streak`, and
reconnects with a 0ms first retry straight back into Dhan's still-active
429 window: the exact instant-429 restart loop this code exists to break,
just through the other boot door.

**The fix:** the fast arm now calls the SAME
`wait_out_persisted_ws_rate_limit_cooldown().await` immediately BEFORE its
`create_websocket_pool` call, mirroring the slow-lane invocation. Semantics
unchanged: fail-open on a missing/corrupt/stale file (no wait), bounded by
`WS_RATE_LIMIT_BACKOFF_CAP_MS` (5 min). Honest envelope: the fast lane's
crash-recovery boot is delayed ONLY when a real persisted 429 cooldown is
active — and then waiting is exactly the desired behaviour (reconnecting
instantly would earn the next 429); worst case is the 5-minute cap.

**Ratchet:** `crates/app/tests/ws_rate_limit_cooldown_wiring_guard.rs` —
source-order scan (house pattern of
`ratchet_tick_processor_spawns_before_reinject_await`) pinning that BOTH
`create_websocket_pool(` call sites in main.rs are preceded by their own
cooldown wait, plus a stub-guard that the wait keeps reading the persisted
file and stays clamped to the cap.

## WS-GAP-09 — pool watchdog reconnected IN PLACE instead of `process::exit`

> **⚠ VARIANT RETIRED in the C4 sweep (2026-07-15):** the `WsGap09WatchdogReconnectInPlace` ErrorCode
> variant is DELETED — its only emit sites (the Dhan main-feed pool watchdog Halt arm) were
> deleted in Phases C2/C3 (#1522/#1569) with the Dhan live WS (operator
> 2026-07-13, `websocket-connection-scope-lock.md` "2026-07-13 Amendment").
> Content below retained for historical audit; the code string stays
> reverse-crossref-allowlisted.

**Trigger:** the pool watchdog reached its `>300s all-down → Halt` verdict, but
the down-cause classified as a **benign bare-Dhan-transport-RST** class rather
than a genuine fatal. Dhan was observed (CloudWatch, 2026-06-30) to silently
TCP-RST the main-feed socket ~5-6s after each connect. The legacy behaviour was
`std::process::exit(2)` → supervisor restart → a full 775-SID cold re-subscribe,
which trips Dhan's per-IP HTTP 429 (the system restarted ~27×/hr). Fix A
(2026-06-30) makes that exit CONDITIONAL: the watchdog consults a pure
`is_bare_reset_class(healths, token_valid, questdb_reachable)` that is true ONLY
when ALL hold — every connection's `rate_limit_streak == 0` (not a real 429), no
connection saw a `NonReconnectableDisconnect`, the token is valid, AND QuestDB is
reachable. When true (and inside the ceiling, below), the watchdog does NOT exit:
it `pool.reset_watchdog()`s (restarts the 300s `AllDown` window), keeps the
per-connection `wait_with_backoff` reconnect loops running (subscriptions
preserved by `SubscribeRxGuard`), and emits this code.

### 2026-06-30 extension — in-window Dhan 429 also rides out in place

The original Fix A above deliberately refused a `rate_limit_streak > 0` (a real
Dhan HTTP 429): `is_bare_reset_class` requires every connection's streak == 0.
But that left the audit's HIGH finding open — an in-MARKET-HOURS 429 still
classified GENUINE-FATAL → `process::exit(2)` → a 775-SID cold re-subscribe →
another 429 → loop (observed 2026-06-30: 61 main-feed 429s in one market window;
the restart loop also 429-starved the independent Groww feed's auth). A 429 is
the EXACT case the per-connection reconnect loops already absorb IN PLACE — they
wait out the `compute_rate_limit_floor_ms` 60s→5m floor (WS-GAP-08) and retry,
subscriptions preserved by `SubscribeRxGuard`; exiting on it is the
self-inflicted harm. So the ride-out decision is widened: the watchdog now
consults `should_reconnect_in_place(healths, in_market_hours, token_valid,
questdb_reachable)` = `is_bare_reset_class(...)` OR a NEW sibling
`is_in_window_429_rideout_class(...)` that admits a 429 ONLY when ALL hold —
`in_market_hours` (09:00–15:30 IST), token valid, QuestDB reachable, NO
connection saw a `NonReconnectableDisconnect`, and at least one connection has
`rate_limit_streak > 0`. Both classes share the SAME genuine-fatal guards and
the SAME 15-min ceiling, so a dead token / dead DB / non-reconnectable code
still exits, and a truly-stuck 429 still falls back to the genuine-fatal restart
after 15 min — never WORSE than today.

This code tags three events:

1. **Benign in-place decision** (`reason="bare_dhan_reset"`) — the common,
   healthy case: a streak==0 bare-RST storm with a valid token + reachable
   QuestDB is ridden out by reconnecting in place, no process restart, no 429.
2. **In-window 429 ride-out** (`reason="in_window_429_ride_out"`) — the
   2026-06-30 extension: an in-MARKET-HOURS Dhan 429 (streak>0) with a valid
   token + reachable QuestDB + no non-reconnectable code is ridden out in place
   (the per-connection loops honor the WS-GAP-08 429 cooldown floor), instead of
   restarting + a cold re-subscribe that just earns the next 429. This is the
   fix for the self-inflicted restart/429 loop.
3. **Ceiling-exceeded fallback** (`reason="ceiling_exceeded"`) — the
   second-tier safety valve (covers BOTH ride-out classes): if reconnect-in-place
   persists past `POOL_RECONNECT_IN_PLACE_CEILING_SECS` (= 900s = **15 minutes**)
   with still zero frame recovery, the watchdog falls back to the genuine-fatal
   `process::exit(2)` (or lane teardown). So the worst case degrades to today's
   behaviour, just after 15 min instead of 5 — strictly never worse.

**Severity:** Low. The benign in-place case is the system self-healing without
operator action; it does NOT page Telegram (the genuine-fatal Halt keeps its
existing `WebSocketPoolHalt` page). The `tv_pool_self_halts_total` counter now
increments ONLY on a genuine-fatal Halt, so its meaning sharpens — a non-zero
rate there is a real restart, not a benign reset ride-out.

**Honest envelope / operator note:** the 15-minute ceiling is a **second-tier
backstop flagged for operator sign-off** — it bounds a worst-case
truly-wedged-feed outage to 15 min (vs today's 5 min) before restarting. It does
NOT stop Dhan's server-side resets (a Dhan support ticket tracks that); it stops
the SELF-INFLICTED restart/429 storm those resets were triggering.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `WS-GAP-09`; the `reason` field is
   `bare_dhan_reset` (streak==0 RST ride-out), `in_window_429_ride_out` (a real
   in-market Dhan 429 ridden out in place — the 2026-06-30 fix), or
   `ceiling_exceeded` (fell back to exit).
2. `tv_ws_watchdog_reconnect_in_place_total{reason}` rate — a sustained
   `bare_dhan_reset` rate during market hours means Dhan is RST-storming; a
   sustained `in_window_429_ride_out` rate means Dhan is 429-storming (check the
   account's per-IP rate limit + the locked 1 main-feed conn). In BOTH cases the
   system is ABSORBING it in place instead of self-restarting. A `ceiling_exceeded`
   increment means a 15-min reconnect-in-place did NOT recover frames → the
   process restarted; treat like the legacy Halt and cross-check token / QuestDB /
   Dhan status.
3. Confirm the feed recovered: `tv_websocket_connections_active` should return to
   the full count once Dhan stops resetting / rate-limiting.

**Auto-triage safe:** YES (Severity::Low; the benign cases already self-healed,
and the ceiling fallback restarts exactly as the legacy Halt did).

**Source:** `crates/app/src/main.rs` (`is_bare_reset_class` +
`is_in_window_429_rideout_class` + `should_reconnect_in_place` +
`reconnect_in_place_ceiling_exceeded` pure helpers + the conditional Halt arm of
`spawn_pool_watchdog_task`), `crates/core/src/websocket/types.rs`
(`ConnectionHealth::{rate_limit_streak, saw_non_reconnectable}`),
`crates/common/src/error_code.rs::WsGap09WatchdogReconnectInPlace`.

