# Implementation Plan: Feed-agnostic self-heal — restart any silently-stalled feed sidecar + ms reconnect

**Status:** APPROVED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — grounded directive (live incident: Groww feed stopped at 10:31 IST and never recovered), code-cited.

Crates touched: **tickvault-app** (`crates/app/src/groww_sidecar_supervisor.rs`,
`crates/app/src/main.rs` wiring), **tickvault-common**
(`crates/common/src/feed_health.rs` — a new read-only getter), plus the
Python sidecar `scripts/groww-sidecar/groww_sidecar.py` (separate process).

> **Self-heal is FEED-AGNOSTIC — any N feeds inherit it with ZERO new per-feed
> code.** The stall-watchdog decision fn takes the inputs for ANY feed (no
> feed-specific branch); the liveness signal is the `Feed`-keyed
> `FeedHealthRegistry` (already common); the supervise loop is parameterized by
> a `Feed` argument. A future feed #3…N registers its own `Feed::ALL` variant +
> sidecar handle and inherits the identical stall→restart + ms-reconnect chain
> with no new watchdog code. Pinned by a generality test that runs the SAME
> decision fn for a novel feed value and asserts the fn has no feed-specific arm.

## Root cause (confirmed, cited — today the Groww feed stopped at 10:31 IST and never recovered)

- Groww NATS server closed the sidecar socket ~10:28 (swallowed "Authorization
  Violation"); `nats-py` closes the connection but NEVER raises.
  `feed.consume()` (`scripts/groww-sidecar/groww_sidecar.py:1141`) is BLOCKING →
  never returns on the dead socket → the Python `except`→reconnect at `:1145`
  never fires.
- Python `silent_feed_watchdog` (`groww_sidecar.py:946-983`) + `nats_reject_poller`
  (`:308-358`) only `print()` — they don't reconnect or exit.
- Rust supervisor `supervise_child` (`crates/app/src/groww_sidecar_supervisor.rs:675-703`)
  restarts ONLY on process exit (`child.wait()`) or feed-disable — there is NO
  "alive-but-silent → restart" arm.
- Reconnect backoff is seconds (Python `RECONNECT_BACKOFF_BASE_SECS=5`/cap 300
  `:382-383`; Rust `sidecar_restart_backoff` first=0 then 2s→60s `:365-371`).

## Plan Items

- [ ] **L2 (PRIMARY) — Rust supervisor stall-watchdog, COMMON per-feed.**
  Add a pure testable `should_restart_on_stall(now_secs, last_tick_at_secs,
  market_open, enabled, threshold_secs) -> bool` (true ONLY when
  `enabled && market_open && last_tick_at known && (now − last_tick_at) >
  threshold`; NO feed-specific branch). Add a 1s-tick `tokio::select!` arm in
  `supervise_child` (parameterized by `feed: Feed`) that reads the feed's
  last-tick age from the `Feed`-keyed `FeedHealthRegistry` and, when the
  decision fn fires, `child.start_kill()` + reaps + returns so the EXISTING
  relaunch/backoff loop respawns the sidecar. Reset `consecutive_failures` on a
  stall restart so the market-hours respawn is immediate (sub-second), while
  genuine crash-loops still back off. Market-hours-gated + edge-triggered (don't
  thrash a legitimately-idle pre-open/closed feed — preserve #1261's
  idle-is-normal). Storm bound: cap rapid stall-restarts in a window — after K
  in the window escalate (`error!` + counter) and apply a backoff ceiling, but
  NEVER permanently give up during market hours.
  - Files: `crates/app/src/groww_sidecar_supervisor.rs`,
    `crates/common/src/feed_health.rs` (read-only `last_tick_age_secs` getter),
    `crates/app/src/main.rs` (pass `Feed::Groww` + `feed_health` + a now-clock
    into `supervise_child`)
  - Tests: `test_should_restart_on_stall_true_when_stale_market_open_enabled`,
    `test_should_restart_on_stall_false_off_hours`,
    `test_should_restart_on_stall_false_disabled`,
    `test_should_restart_on_stall_false_fresh`,
    `test_should_restart_on_stall_false_no_tick_yet`,
    `test_should_restart_on_stall_boundary_exactly_at_threshold`,
    `test_should_restart_on_stall_is_feed_agnostic_for_novel_feed`,
    `test_stall_restart_reuses_existing_relaunch_path` (source-scan seam),
    `test_stall_restart_storm_is_bounded` (storm window cap),
    `feed_health::tests::test_last_tick_age_secs_*`

- [ ] **L2 — ErrorCode + counter for the stall restart (feed-agnostic label).**
  `tv_feed_sidecar_stall_restart_total{feed}` static-label counter +
  `warn!`/`error!` carrying an ErrorCode. Reuse the GROWW-MASTER family pattern;
  add `FEED-STALL-01` (`ErrorCode::FeedStall01SidecarRestarted`,
  Severity::High, NOT auto-triage-safe) + a rule-file mention so the cross-ref
  + tag guards pass. The storm-escalation logs `error!(code=FEED-STALL-01)` with
  the rapid-restart count.
  - Files: `crates/common/src/error_code.rs`,
    `.claude/rules/project/feed-stall-watchdog-error-codes.md`,
    `crates/app/src/groww_sidecar_supervisor.rs`
  - Tests: `error_code::tests` (existing meta-guards exercise the new variant);
    `crates/common/tests/error_code_rule_file_crossref.rs` (auto-covers via the
    new rule file)

- [ ] **L2 — supervise the watchdog itself (don't let it die silently).**
  The stall arm runs INSIDE `supervise_child`, which already runs inside the
  top-level `run_groww_sidecar_supervisor` loop; that loop is the supervisor. To
  ensure the supervisor TASK itself can't die silently, wrap its spawn in a
  respawning supervisor in `main.rs` (mirror WS-GAP-05 / DISK-WATCHER-01): a thin
  `tokio::spawn` wrapper that, on JoinHandle completion (panic / unexpected
  return), logs `error!` + increments `tv_feed_supervisor_respawn_total{feed}` +
  re-spawns. Pure decision fn `should_respawn_supervisor(join_outcome) -> bool`.
  - Files: `crates/app/src/main.rs`, `crates/app/src/groww_sidecar_supervisor.rs`
  - Tests: `test_should_respawn_supervisor_on_panic_or_return`,
    `test_should_respawn_supervisor_*`

- [ ] **L1 — Python sidecar: break the silent consume + ms reconnect.**
  Make `silent_feed_watchdog` ACTIVELY recover: when decoded records stall (0 new
  in a short `STALL_DEADLINE_SECS`) DURING MARKET HOURS, FORCE `consume()` to
  return by closing the NATS socket via the already-reached `_nats_socket_from_feed`
  handle (a best-effort `close`), so the `except`→reconnect at `:1145`
  re-subscribes the full watch-list. Market-hours reconnect ladder
  50ms→100ms→…→cap ~5s (`_backoff_secs` gains a market-open fast path), NEVER
  give up while `[09:15,15:30) IST`; keep the longer off-hours ladder + the
  rate-limit (60s) ladder. Never `sys.exit` while market-open (reconnect, don't
  kill — L2 is the kill backstop). Pure extractable decision functions
  (`_is_market_open_ist`, `should_force_reconnect`, `_backoff_secs`) are
  unit-tested via `--selftest`.
  - Files: `scripts/groww-sidecar/groww_sidecar.py`
  - Tests: `_selftest_*` assertions inside `groww_sidecar.py --selftest`
    (extractable pure decision fns), invoked by a Rust source-scan test that
    pins the selftest exists + the force-reconnect path is wired.

- [ ] **FIX 0 — archive the merged Day-1 plan; replace active-plan.md.** The
  stale `active-plan.md` belonged to the already-merged Groww Day-1 lifecycle
  audit work and lacked relevance to this task. Archived to
  `.claude/plans/archive/2026-06-29-groww-day1-lifecycle-audit-fresh-db.md`;
  this file is the new plan.
  - Files: `.claude/plans/active-plan.md`,
    `.claude/plans/archive/2026-06-29-groww-day1-lifecycle-audit-fresh-db.md`
  - Tests: `bash .claude/hooks/plan-gate.sh "$PWD"`,
    `cargo test -p tickvault-storage --test per_item_guarantee_matrix_guard`

## Design

The operator requirement (forever, ALL feeds, all worst cases): a LIVE feed must
NEVER stay disconnected during market hours; on any drop it reconnects within
seconds and re-subscribes; only the rarest extreme is down at all. Two
defence-in-depth layers, both FEED-AGNOSTIC:

1. **L2 (Rust, PRIMARY) — alive-but-silent → restart.** The supervisor already
   owns the child process AND already receives the `Feed`-keyed
   `FeedHealthRegistry` (the Groww bridge stamps `last_tick_ist_nanos[Groww]` via
   `record_ticks` on every persisted batch). The supervisor's `supervise_child`
   today only wakes on `child.wait()` (process exit) or the disable poll — it is
   BLIND to a child that is alive but streaming nothing. We add a 1s-tick
   `select!` arm that polls the feed's last-tick AGE and, when
   `should_restart_on_stall(...)` is true, kills + reaps the child and returns
   `false` so the EXISTING relaunch loop respawns it. This is the robust backstop
   because it does NOT depend on the fragile NATS internals — it works for ANY
   sidecar-backed feed that records ticks into the registry. Feed-agnostic by
   construction: `supervise_child` takes a `feed: Feed`, the registry is keyed by
   `Feed`, and the decision fn has no feed-specific branch.

2. **L1 (Python) — break the blocking consume + ms reconnect.** L1 makes the
   sidecar self-heal faster than L2's kill-and-relaunch in the common case: the
   silent-feed watchdog (already running, already holding the socket handle) now
   force-closes the dead NATS socket so the blocking `consume()` returns and the
   `except`→reconnect re-subscribes — at ms cadence during market hours. L1 and
   L2 are complementary: L1 is the fast in-process recovery; L2 is the
   process-level kill backstop that fires if L1 fails (e.g. the socket handle is
   unreachable on a future wheel, or the consume hangs in C).

3. **ILLIQUID-vs-DEAD (honest, the key correctness point).** A genuinely quiet
   instrument has no ticks but the socket is ALIVE — restarting it would be
   wrong. We restart on a FEED-LEVEL last-tick across the WHOLE universe (~767
   SIDs), NOT a per-instrument gap. At market open, ticks flow every second
   across the universe (the NTM union of 765 stocks + 2 indices), so a
   feed-level stall > threshold during market hours = a real dead socket, not
   illiquidity. We pick a generous `FEED_STALL_RESTART_SECS` (default 30s, the
   same order as `FEED_STALE_TICK_SECS`) so a brief lull never false-restarts,
   yet a truly dead socket (today's 10:31→silence-forever) is caught within ~30s.
   We additionally require `last_tick_at` to be KNOWN (a tick was seen at least
   once this session) before the stall arm can fire, so a feed that has not yet
   delivered its first tick (cold pre-open) is NEVER killed by this arm — it is
   covered by the existing silent-feed diagnostic + the L1 watchdog, not a kill
   loop.

4. **Feed-level liveness signal.** `FeedHealthRegistry::last_tick_age_secs(feed,
   now_ist_nanos) -> Option<u64>` (NEW, read-only, O(1)) returns the age of the
   most-recent tick across the feed's whole universe, or `None` if no tick yet.
   The supervisor reads this each 1s tick. No new wiring — the bridge already
   stamps it.

## Edge Cases (incl. EXTREME / out-of-box / combination)

- **Basic stall (today's incident):** socket dies 10:31, no ticks; within ~30s
  the L2 watchdog kills + relaunches → re-auth → re-subscribe → ticks resume.
- **Illiquid-vs-dead:** a single quiet SID has no ticks but the feed-level
  last-tick (across 767 SIDs) is fresh → NO restart. Only a feed-level gap
  > threshold during market hours restarts. Documented + pinned by the
  threshold + the feed-level age getter.
- **No first tick yet (cold pre-open):** `last_tick_age_secs` is `None` →
  `should_restart_on_stall` returns false (the arm requires a known last-tick).
  Never kills a feed that hasn't streamed its first tick.
- **(1) RESTART STORM / flapping:** a socket that reconnects then immediately
  re-drops repeatedly. Bounded: a sliding window counts rapid stall-restarts;
  after `STALL_RESTART_STORM_MAX` in `STALL_RESTART_STORM_WINDOW_SECS`, the
  watchdog escalates (`error!(code=FEED-STALL-01)` + counter) and applies a
  backoff CEILING (does NOT reset `consecutive_failures` to 0 for that restart,
  so the existing `sidecar_restart_backoff` ceiling of 60s applies) — but NEVER
  permanently gives up during market hours (it keeps retrying at the ceiling).
  Tested by `test_stall_restart_storm_is_bounded`.
- **(2) Illiquid-vs-dead:** covered above (feed-level signal + threshold).
- **(3) BOTH feeds stall simultaneously:** each feed has its OWN `supervise_child`
  loop reading its OWN `Feed`-keyed registry slot → each restarts independently,
  no cross-interference (per-feed by construction; the registry is a fixed
  per-`Feed` array).
- **(4) Stall exactly AT 09:15:00 open / 15:30:00 close boundary:** the
  market-hours gate uses the existing `is_within_market_hours_ist()`
  (start-inclusive, end-exclusive per `TICK_PERSIST_*` window). At/after
  15:30:00 `market_open` is false → `should_restart_on_stall` returns false → no
  restart at close. Edge pinned by `test_should_restart_on_stall_false_off_hours`
  + the existing market-hours window tests.
- **(5a) Token-expiry + socket-drop together:** L2 kills + relaunches; the
  relaunch path re-auths (Python `access_token=None` on auth-class failure) so a
  combined fault self-heals on the next cycle. No special-casing needed.
- **(5b) QuestDB-down + feed-stall together (must NOT lose buffered ticks):**
  the capture-at-receipt NDJSON file is the durable floor. The sidecar fsyncs
  each tick to `data/groww/live-ticks.ndjson` the instant the callback fires,
  and the Rust BRIDGE tails that file INDEPENDENTLY of the sidecar process (it
  re-opens the file each wake, tracks a byte `offset`, drains only complete
  newline-terminated lines — `groww_bridge.rs:582-633`). So killing the sidecar
  does NOT drop the in-flight buffer: the bridge re-reads from its offset after
  the relaunched sidecar continues appending. (If QuestDB is down, the bridge's
  ring→spill→DLQ chain absorbs the rows — unchanged by this PR.) Verified by the
  existing `drain_complete_prefix` partial-line tolerance.
- **(6) Watchdog/supervisor task itself panics:** the supervisor task is wrapped
  in a respawning `tokio::spawn` supervisor in `main.rs` (WS-GAP-05 pattern):
  on JoinHandle panic/return it logs `error!` + `tv_feed_supervisor_respawn_total`
  + re-spawns. The watchdog can't die silently. Pinned by
  `test_should_respawn_supervisor_*`.
- **(7) Operator DISABLES the feed mid-stall:** the disable path WINS. The
  `should_restart_on_stall` fn requires `enabled` true; the supervise loop's
  disable poll (`SIDECAR_DISABLE_POLL`) already returns `true` (graceful) when
  `!feed_runtime.is_enabled(feed)`. The stall arm re-reads `enabled` each tick,
  so a disable toggle short-circuits the kill (the watchdog does not fight the
  toggle). Tested by `test_should_restart_on_stall_false_disabled`.
- **(8) Sidecar killed mid-write / partial NDJSON line:** the bridge's
  `drain_complete_prefix` drains only up to the last newline; a trailing partial
  line (incl. a partial UTF-8 char) stays buffered for the next wake — no
  corruption. This is EXISTING behaviour (`groww_bridge.rs:630-632`), preserved.
- **(9) Clock skew / NTP step:** `last_tick_age_secs` uses
  `now_ist_nanos.saturating_sub(last_tick).max(0)` — a BACKWARD clock step yields
  age 0 (clamped), so a backward step can never false-trigger a stall restart. A
  forward step inflates the age once; the next real tick resets it. Honest
  envelope: a large forward NTP step during a genuine lull could trigger one
  early restart — acceptable (a restart is cheap + recovers; it never gives up).
  We use the wall-clock IST-nanos basis the registry already uses (consistent
  with the snapshot path) rather than a second time source.
- **(10) IST-midnight rollover during a stall:** `is_within_market_hours_ist()`
  is false across midnight (outside `[09:00,15:30)`), so no restart fires
  overnight. At the next open, the cold-pre-open guard (no-first-tick → no
  restart) applies until the first tick. No spurious restart at rollover.
- **(11) Rate-limit on reconnect (Groww token endpoint):** L1 keeps the existing
  rate-limit ladder (`RATE_LIMIT_BACKOFF_BASE_SECS=60`) — a 429 in the [auth]
  phase backs off 60s, NOT the ms ladder. L2's relaunch hits the SAME Python
  reconnect logic, so a rate-limited token call still respects the 60s ladder —
  L2 does not hammer the endpoint (L2 kills the PROCESS, the relaunched process
  re-enters the rate-limit-aware Python loop). The L2 storm bound (case 1)
  additionally caps L2's own relaunch rate.

## Failure Modes

- **NATS socket handle unreachable on a future wheel (L1 can't force-close):**
  `_nats_socket_from_feed` returns None → L1 force-reconnect degrades to a
  print-only watchdog (as today), BUT L2 (the process kill) still fires within
  ~30s. Defence-in-depth: L2 does not depend on NATS internals.
- **`consume()` hangs in C (signal won't interrupt):** L1's socket-close may not
  unblock a C-level hang; L2's `child.start_kill()` (SIGKILL on drop via
  `kill_on_drop` + explicit kill) terminates the process unconditionally → relaunch.
- **Misclassifying an illiquid lull as dead:** prevented by the feed-level
  (whole-universe) signal + the 30s threshold + the known-last-tick requirement.
  A 30s feed-level gap across 767 SIDs during market hours is not illiquidity.
- **Watchdog flaps (kills a recovering feed before its first post-restart tick):**
  prevented by resetting the stall clock on restart (a freshly-relaunched feed
  has `last_tick_age_secs` based on the last PRE-kill tick; the storm bound +
  the requirement that a NEW stall window elapse before the next kill prevent a
  tight kill loop). The storm escalation caps it.
- **Supervisor respawn loop (case 6 wrapper itself misbehaves):** the respawn
  wrapper backs off (reuses `sidecar_restart_backoff` shape) + counts respawns;
  it never tight-loops.
- No new allocation, no new lock on any hot path — the stall arm is a 1s-cadence
  cold-path poll of one relaxed atomic load; L1 is a separate process.

## Test Plan

Pure decision functions make L2 fully unit-testable without a live sidecar or clock:
- `should_restart_on_stall` — the 7 cases (stale+open+enabled→true; off-hours;
  disabled; fresh; no-tick-yet; exactly-at-threshold boundary; **feed-agnostic
  generality** running the SAME fn for an arbitrary/novel feed input).
- `should_respawn_supervisor` — panic/return → true, clean-disable-return → as designed.
- `last_tick_age_secs` getter — None when no tick; correct age; clamp on backward clock.
- A storm test asserts the rapid-restart window cap escalates + applies the ceiling.
- A source-scan seam test asserts the stall path reuses the existing relaunch
  (returns `false` from `supervise_child` → the existing loop relaunches) so a
  refactor can't silently break it.
- L1: the extractable pure decision fns are asserted inside
  `groww_sidecar.py --selftest`; a Rust source-scan test pins the selftest +
  the force-reconnect wiring exist.
- Run `cargo test -p tickvault-app` + `cargo test -p tickvault-common` +
  `cargo test -p tickvault-storage --test per_item_guarantee_matrix_guard` +
  `cargo test -p tickvault-common --test error_code_rule_file_crossref`.
  banned-pattern + pub-fn-guard + pre-push-gate clean. Do NOT regress #1261
  (idle=INFO off-hours) or the existing supervisor tests.

**Honestly un-unit-testable live:** the actual NATS socket-close→consume-return
behaviour (depends on the live growwapi/nats-py wheel) and the real
kill→relaunch→re-auth→re-subscribe round trip are integration-only — they are
covered by the pure decision fns + source-scan seams here, and verified live
against the running feed after merge. Stated honestly per the charter envelope.

## Rollback

Contained to one supervisor module + one read-only registry getter + one
ErrorCode + one rule file + the separate Python sidecar. To roll back: revert
the single PR. No schema change, no migration, no live-data tick/dedup/persist
or order path touched (the watchdog only KILLS + RELAUNCHES the sidecar process;
the durable NDJSON floor + bridge are unchanged). A revert returns the system to
the pre-PR behaviour (a silently-stalled sidecar stays dead) — never to a worse
state.

## Observability

- NEW `tv_feed_sidecar_stall_restart_total{feed}` counter (static label) — one
  increment per stall-driven restart, per feed.
- NEW `tv_feed_supervisor_respawn_total{feed}` counter — one per supervisor-task
  respawn (case 6).
- NEW `FEED-STALL-01` (`ErrorCode::FeedStall01SidecarRestarted`, Severity::High)
  — `warn!`/`error!` on each stall restart; `error!` on the storm escalation
  with the rapid-restart count. Routed to Telegram via the 5-sink error pipeline.
  Rule-file runbook `.claude/rules/project/feed-stall-watchdog-error-codes.md`.
- After this PR, a silently-stalled live feed (any feed) is killed + relaunched
  within ~30s during market hours, and the operator sees WHY via the counter +
  the FEED-STALL-01 log — never again a feed that "stopped at 10:31 and never
  came back".

## Plan Items

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Socket dies mid-session, 0 ticks, market open | L2 kills + relaunches within ~30s; ticks resume |
| 2 | One illiquid SID quiet, feed-level ticks fresh | NO restart (feed-level signal fresh) |
| 3 | Cold pre-open, no first tick yet | NO restart (last-tick unknown) |
| 4 | Flapping socket (reconnect→re-drop repeatedly) | storm bound escalates + applies ceiling, never gives up in market hours |
| 5 | Both feeds stall at once | each restarts independently (per-feed map) |
| 6 | Stall at 15:30:00 close | NO restart (market_open false) |
| 7 | Token-expiry + socket-drop together | relaunch re-auths + re-subscribes |
| 8 | QuestDB-down + feed-stall | NDJSON floor preserved; bridge re-reads from offset; no buffered-tick loss |
| 9 | Watchdog/supervisor task panics | respawned (case-6 wrapper) + counter |
| 10 | Operator disables feed mid-stall | disable wins; watchdog does not fight the toggle |
| 11 | Sidecar killed mid-write (partial NDJSON line) | bridge drains to last newline; no corruption |
| 12 | Backward NTP clock step during a lull | age clamped to 0; no false restart |
| 13 | IST-midnight rollover during a stall | off-hours gate → no spurious restart |
| 14 | Rate-limited token endpoint on reconnect | 60s rate-limit ladder respected; not hammered |
| 15 | A novel future feed (feed #3) | inherits the identical stall→restart with ZERO new code (generality test) |

## Per-item guarantee matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 rows of the
15-row "100% everything" matrix and all 7 rows of the Resilience Demand Matrix
apply to every item in this plan. Honest envelope: the changes are confined to
the **cold-path sidecar supervisor** (a 1s-cadence poll) + a **read-only O(1)
registry getter** + the **separate Python process** — they add NO hot-path
allocation and touch NO tick / dedup of live data / persist-of-live-data / order
path, so the DHAT zero-alloc and Criterion p99 rows are `N/A — cold-path
supervisor poll + read-only atomic load`. Coverage is proven by the pure-function
unit tests named per item; composite-key uniqueness
`(security_id, exchange_segment, feed)` per I-P1-11 and the zero-tick-loss
ring→spill→DLQ chain are unchanged (the kill→relaunch preserves the durable
capture-at-receipt NDJSON floor). Any honest "100%" claim in this plan is 100%
inside the tested envelope, with ratcheted regression coverage by those unit
tests + the source-scan seams; the live NATS-close→reconnect round trip is
integration-verified after merge (stated honestly).
