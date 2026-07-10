# Feed-Agnostic Sidecar Stall-Watchdog — Error Codes (FEED-STALL-01 / FEED-SUPERVISOR-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `websocket-connection-scope-lock.md` (the 2-Dhan-WS lock is untouched) > this file.
> **Operator directive (2026-06-30, live incident):** the Groww feed stopped at
> 10:31 IST and never recovered (a silently-closed NATS socket left the sidecar
> ALIVE but streaming nothing; the blocking `feed.consume()` never returned, the
> Python `except`→reconnect never fired, and the Rust supervisor only restarted on
> process EXIT — there was no "alive-but-silent → restart" arm). *"a live feed must
> NEVER stay disconnected during market hours; on any drop it reconnects within
> seconds and re-subscribes — and this must be FEED-AGNOSTIC: any future feed
> inherits the same self-heal with zero new code."*
> **Companion code:** `crates/app/src/groww_sidecar_supervisor.rs`
> (`should_restart_on_stall`, the `supervise_child` stall arm, the storm bound,
> the respawning supervisor wrapper), `crates/common/src/feed_health.rs`
> (`FeedHealthRegistry::last_tick_age_secs` — the feed-level liveness signal),
> `crates/common/src/error_code.rs::ErrorCode::{FeedStall01SidecarRestarted,
> FeedSupervisor01Respawned}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `FeedStall01*` / `FeedSupervisor01*` variant verbatim
> — `FEED-STALL-01` and `FEED-SUPERVISOR-01` appear below.

---

## §0. Why these codes exist (the locked, feed-agnostic design)

A live-feed sidecar can be ALIVE-but-silent: the process is running, but its
upstream socket was closed by the provider without raising, so it streams zero
ticks forever. Detection cannot rely on process exit (the process is fine) — it
must rely on a FEED-LEVEL liveness signal. Both codes are FEED-AGNOSTIC: the
decision function takes the inputs for ANY feed (no feed-specific branch), the
liveness signal is the `Feed`-keyed `FeedHealthRegistry`, and the supervise loop
is parameterized by a `Feed` argument. A future feed #3…N inherits the identical
stall→restart + supervisor-respawn chain with ZERO new code.

**ILLIQUID-vs-DEAD (the key correctness point):** a single quiet instrument has
no ticks but the socket is ALIVE — restarting it would be wrong. The watchdog
restarts on a FEED-LEVEL last-tick across the WHOLE subscribed universe (~767
SIDs for Groww), NOT a per-instrument gap. At market open ticks flow every second
across the universe, so a feed-level stall > `FEED_STALL_RESTART_SECS` during
market hours = a real dead socket, not illiquidity. The arm additionally requires
a KNOWN last-tick (a tick seen at least once this session), so a feed that has
not yet streamed its first tick (cold pre-open) is NEVER killed by this arm.

---

## §1. FEED-STALL-01 — silently-stalled sidecar killed + relaunched

**Severity:** High. **Auto-triage safe:** Yes (the restart already self-healed;
the operator inspects the storm count at leisure — but a flapping STORM is the
operator-action signal: it points at a persistent provider-side reject the
relaunch alone cannot fix).

**Trigger:** `should_restart_on_stall(now, last_tick_at, market_open, enabled,
threshold)` returned true inside `supervise_child` — the feed was enabled, the
market was open, a last-tick was known, and the feed-level last-tick age exceeded
`FEED_STALL_RESTART_SECS`. The watchdog `child.start_kill()`s + reaps the sidecar
and returns so the existing relaunch/backoff loop respawns it (re-auth +
re-subscribe). `tv_feed_sidecar_stall_restart_total{feed}` increments.

**Restart STORM bound:** a sliding window counts rapid stall-restarts. After
`STALL_RESTART_STORM_MAX` in `STALL_RESTART_STORM_WINDOW_SECS`, the watchdog
escalates (`error!(code=FEED-STALL-01, rapid_restarts=N)`) and applies a backoff
CEILING (the existing `sidecar_restart_backoff` 60s cap), but NEVER permanently
gives up during market hours — it keeps retrying at the ceiling. A persistent
storm means the provider keeps closing the socket (entitlement / auth reject) —
operator must check the credential / entitlement.

**2026-07-06 (corrected in round-3 review):** two pagers now exist, and the
emit LEVELS matter — the PER-RESTART emission is `warn!`
(`groww_sidecar_supervisor.rs`, the non-storm arm); only the STORM escalation
(the 6th+ rapid restart, `>STALL_RESTART_STORM_MAX=5` within 300s) is
`error!`. The ERROR-only errors.jsonl sink therefore never carries individual
restarts:

1. **`tv-<env>-feed-stall-restarts`** (`feed-stall-restart-alarm.tf`) — the
   restart pager: a log metric filter on `/tickvault/<env>/metrics` extracts
   the per-scrape deltas of `tv_feed_sidecar_stall_restart_total` (increments
   once per restart, warn! + error! alike; pre-registered at 0 in main.rs
   immediately AFTER the metrics recorder installs at boot — round-5 fix
   2026-07-06, ratcheted by
   `test_stall_restart_counter_is_preregistered_after_recorder_install`; the
   round-4 supervisor-spawn registration was VOID because the supervisor is
   spawned before the recorder installs, so its handle resolved to the no-op
   recorder — so the CW agent's dropped-first-sample delta baseline is the
   harmless 0, NOT restart #1 of the session; without a post-install
   registration the first restart of every app session was uncounted and the
   effective first-episode threshold was 4), Sum ≥ 3 per aligned 15-min window
   → SNS → Telegram. Honest floor (span math, round-12): 3 restarts span 2
   gaps, so cycles faster than ~5 min page promptly, the ~5-7.5 min band
   pages eventually via phase drift of a sustained flap, and only cycles
   slower than ~7.5 min (>450s) can never fit 3 inside one aligned 900s
   window — stated residual.
2. **`tv-<env>-errcode-feed-stall-01`** (`error-code-alarms.tf`) — the
   storm-escalation tripwire: ONE storm-arm ERROR line pages (Sum ≥ 1 per
   5 min; the Rust detector already debounces at >5 restarts/5 min). It
   canNOT see 3-5 restarts/15 min — those lines are warn!-level; that band is
   pager #1's job.

A single benign self-heal restart never pages on either route (this section's
own operator-action bound). Before 2026-07-06 even the storm `error!` reached
only the log sinks — the error!→Telegram route was severed by the
CloudWatch-only migration.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `FEED-STALL-01`; the payload carries
   `feed` + (on the storm escalation) `rapid_restarts`.
2. `tv_feed_sidecar_stall_restart_total{feed}` rate — a steady non-zero rate
   during market hours means the socket keeps dying; cross-check the sidecar's
   stderr (`GROWW LIVE FEED REJECTED: …` / `SILENT FEED …` lines) for the
   provider reason.
3. A single restart that recovers (ticks resume, count stops) = healthy
   self-heal, no action. A STORM (escalation fired) = persistent reject — verify
   the Groww SSM api-key + the account's live-feed entitlement.

**Honest envelope:** the kill→relaunch preserves the durable capture-at-receipt
NDJSON floor — the bridge tails `data/groww/live-ticks.ndjson` INDEPENDENTLY of
the sidecar process (tracks a byte offset, drains only complete newline-terminated
lines), so killing the sidecar never drops the in-flight buffer. The watchdog
restores the SOCKET; it does not claim the provider will never re-reject.

**Source:** `crates/common/src/error_code.rs::ErrorCode::FeedStall01SidecarRestarted`,
`crates/app/src/groww_sidecar_supervisor.rs` (`should_restart_on_stall`, the
`supervise_child` stall `select!` arm, the storm bound).

**2026-07-10 update (dual-feed scoreboard PR-B):** every stall-watchdog
kill+relaunch (this arm AND the §1b never-streamed arm, storm and non-storm
alike) ALSO stamps ONE `WsEventKind::StallRestarted` (`stall_restarted`)
`ws_event_audit` forensic row — best-effort try_send through the same
consumer the bridge lifecycle rows use (failure = AUDIT-WS-01, never a
supervision stall). The row's `source` carries a FIXED machine cause slug
(`stall_silent_socket` / `stall_never_streamed` / `stall_auth_stale` /
`stall_entitlement` — the drains latch the child's last CONFIRMED
auth/entitlement reject class; generic `Error` lines never latch; a
streaming recovery clears the latch), `down_secs` carries the silent
window. The 15:45 IST scoreboard counts these as stall episodes with blame
(`dual-feed-scoreboard-error-codes.md` §2). Counters + pagers UNCHANGED.

### §1b. 2026-07-09 Update — NEVER-STREAMED restart arm (`reason = "never_streamed"`)

**The incident this closes:** the Groww feed paged `[HIGH] Groww live feed
rejected — the feed reported an error and is retrying; connected but receiving
nothing` at 09:22 IST AND again at 14:17 IST on 2026-07-09 — an all-day
recurring `SidecarLineClass::Error` loop. The §0 ILLIQUID-vs-DEAD rule
requires a KNOWN last-tick (`should_restart_on_stall` returns `false` on
`age == None`), so a sidecar that is alive + connected + subscribed but NEVER
streamed its FIRST tick this session was NEVER stall-restarted — the verified
"unknown last-tick → never restarted" hole.

**The new arm:** `supervise_child` now tracks a per-child never-streamed
window. When the feed is enabled, NO first tick has ever been recorded this
session, AND the wall clock is inside the EXCHANGE session — [09:15, 15:30)
IST via `g1_exchange_gate_accepts` AND a trading day via
`is_trading_session_now()` (weekday + NSE-holiday aware; deliberately NOT the
plain time-of-day gate the stall arm uses, which is `true` on a Saturday
10:00 IST and would have restart-looped every weekend) — for longer than
`FEED_NEVER_STREAMED_RESTART_SECS` (300s, strict `>`), the child is killed +
relaunched exactly like a stall restart (pure decision:
`should_restart_on_never_streamed`; same `StallRestartStorm` bound; same
`SuperviseOutcome` relaunch seam). Earliest possible fire is therefore
09:20:05 IST — a healthy feed streams within seconds of the 09:15 open across
the ~770-SID universe. Each relaunched child gets the full 300s again, so a
persistently never-streaming feed restarts at a bounded ~5-minute cadence.
The window clears permanently the instant a first tick arrives (the classic
stall arm owns liveness from then on) and clears whenever the session gate
closes, so pre-open / after-close / weekend / holiday silence can never arm
it. The disable toggle still wins (re-read inside the arm).

**Emissions:** `warn!(code = "FEED-STALL-01", reason = "never_streamed",
silent_session_secs)` per restart (storm edge → `error!`, exactly the §1
level split — the `tv-<env>-errcode-feed-stall-01` alarm semantics are
UNCHANGED). Counters: the EXISTING pre-registered
`tv_feed_sidecar_stall_restart_total{feed}` also increments (so the
`tv-<env>-feed-stall-restarts` ≥3-per-15-min pager fires on a persistent
never-streamed loop — the operator's "feed keeps failing after restarts"
signal) PLUS the NEW attribution counter
`tv_feed_sidecar_never_streamed_restart_total{feed}` (pre-registered in
main.rs next to the stall counter, same delta-baseline rationale).

**Honest envelope (incl. the 2026-07-09 post-impl hostile-review outcomes):**
the restart re-auths + re-subscribes; it cannot force Groww's server to send
data. A server-side reject that persists shows up as the bounded restart
cadence + the pager + the §1c FEED-REJECT-01 signatures — loud, never silent,
never a tight kill loop. Known bounds, stated plainly:

- **Page-storm bound (review HIGH, FIXED):** each relaunched child gets a
  fresh `alerted` latch, so a persistent reject day would have paged the
  `GrowwSidecarRejected` HIGH ~12×/hour. The single-conn Telegram fan-out is
  now additionally gated by a supervisor-lifetime cross-child cooldown
  (`GROWW_REJECT_PAGE_COOLDOWN_SECS` = 1800s, pure `should_page_reject`,
  CAS'd so the two pipe drains never double-page): at most one reject page
  per 30 min per supervisor; suppressed episodes keep their `error!`
  forwards, FEED-REJECT-01 signature, and feed-health marking, and are
  counted by `tv_groww_reject_page_cooldown_suppressed_total`. The
  ≥3-per-15-min restart pager independently covers "it keeps failing".
- **"This session" = this APP PROCESS:** `feed_health`'s last-tick stamp
  never resets in-process, so on day 2 of a long-running process the arm is
  dormant and the classic 30s stall arm owns everything (no coverage hole).
  The arm's value is the boot/deploy-into-contention morning.
- **Token-stale interplay (review MEDIUM, DOCUMENTED):** during an in-session
  auth-stale episode the ~5-min relaunch resets the sidecar's 600s
  `access token stale` escalation clock, so that specific minter-dead marker
  may never print in-session. Compensating signals: every failed auth cycle
  still prints `groww sidecar error [auth]: …` (AuthRejected class → the
  specific "authentication rejected" Telegram wording + feed-health RED),
  and the FEED-REJECT-01 signature carries the `error [auth]` prefix.
  Restarting is harmless to the token itself (the sidecar only ever READS
  SSM — never mints).
- **Connection churn (review MEDIUM, DOCUMENTED):** ~12 extra Groww sessions
  per hour from the relaunch cadence is marginal against the sidecar's own
  in-process reconnect ladder (5s→300s backoff, up to ~720 fresh
  connections/hour during a failure loop); if the shared account is in a
  churn-penalty window, the dominant churn source is the internal ladder,
  not this arm.

### §1c. FEED-REJECT-01 — bounded sidecar reject-cause signature (2026-07-09)

**Severity:** High. **Auto-triage safe:** Yes (visibility only — the
restart/backoff machinery already owns recovery).
`ErrorCode::FeedReject01SidecarErrorDetail` (`code_str() == "FEED-REJECT-01"`).

**Why it exists:** the operator-facing Telegram reason is a FIXED string per
`SidecarLineClass` (correct per the 10 Telegram commandments — raw child text
never reaches Telegram), but before 2026-07-09 NO coded capture of the
triggering sidecar line reached the error stream — so during the all-day
reject loop neither the operator nor triage could query errors.jsonl /
CloudWatch for WHY the loop repeated (the per-line forwards are uncoded and
buried).

**Trigger:** the once-per-running-child alert edge in `spawn_pipe_drain` (the
same latch that fires the `GrowwSidecarRejected` Telegram) now ALSO emits ONE
`error!(code = "FEED-REJECT-01", feed, class, signature)` where `signature` =
`sidecar_line_signature(line)` — the (already sidecar-redacted) line piped
through the existing `capture_rest_error_body` sanitize choke point
(control-char strip → URL/credential-param redaction → JWT-shape redaction →
credential-JSON-field redaction) then truncated to
`SIDECAR_LINE_SIGNATURE_MAX_CHARS` (160 chars, UTF-8-safe; BiDi/zero-width/BOM
chars also stripped per the 2026-07-09 security review). Bounded by its OWN
per-child `detail_logged` latch — once per child episode on EVERY path,
re-armed only by a streaming recovery, deliberately NOT by the fleet
Suppress re-arm of `alerted` (review MEDIUM fix: the re-arm would have made
this emit per-line on the fleet path). Telegram wording is UNCHANGED.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `FEED-REJECT-01`; the
   `signature` field names the cause directly (see the sidecar
   stderr-signature map below): `error [auth]` = token/SSM;
   `error [feed-connect]` / `error [subscribe]` / `error [consume]` = the
   SDK/NATS phase + exception type + redacted detail; `SILENT FEED` /
   `STILL SILENT` = server-side session starvation; `Authorization
   Violation` / `Permissions Violation` = NATS-level reject.
2. Correlate with `tv_feed_sidecar_stall_restart_total` +
   `tv_feed_sidecar_never_streamed_restart_total` — repeating signatures
   across restarts = a persistent provider-side condition the relaunch alone
   cannot fix.
3. **Delivery boundary (honest):** FEED-REJECT-01 is log-sink-only today (no
   `error_code_alerts` map entry) — the operator PAGE for the episode is the
   existing `GrowwSidecarRejected` Telegram; the coded line is the forensic
   WHY. Adding a `tv-<env>-errcode-feed-reject-01` filter is a flagged
   follow-up (one map entry in `deploy/aws/terraform/error-code-alarms.tf`).

**Sidecar stderr-signature map (what `classify_sidecar_line` sees, verified
2026-07-09 against `scripts/groww-sidecar/groww_sidecar.py`):**

| Sidecar stderr line shape | Class | Telegram reason (fixed) | Meaning |
|---|---|---|---|
| `groww sidecar error [auth]: <Type>: <detail>` | AuthRejected | "authentication rejected — the shared access token is stale/unusable…" | SSM read / boto3 / token-shape failure in the auth phase |
| `GROWW LIVE FEED REJECTED: access token stale for Ns (>10min)…` | AuthRejected | same | 10-min continuous auth-stale marker (minter Lambda dead) |
| `groww sidecar: SILENT FEED — subscribed N stocks + M indices but received NO live records in 30s…` | EntitlementRejected (market-hours-gated) | "server is not sending data to this connection (session limit or throttle) — retrying with backoff" | subscribed-but-silent watchdog |
| `groww sidecar: STILL SILENT — 0 live records decoded…` | EntitlementRejected | same | periodic silent reminder |
| `GROWW LIVE FEED REJECTED: AuthorizationError…` / any line carrying `authoriz` / `permission` / `entitlement` | EntitlementRejected | same | NATS `-ERR` reject surfaced from the socket's `last_error` |
| `groww sidecar error [feed-connect / subscribe / consume / rotate / rotate-retention]: <Type>: <detail>` | **Error** | **"the feed reported an error and is retrying"** ← today's 09:22 / 14:17 wording | reconnect-loop exception in that phase (SDK / NATS / protobuf / IO) |
| `groww sidecar error [<phase>] traceback (failure #N): …` | Error | same | redacted traceback (1st + every 100th failure) |
| SDK/NATS logger lines matching ` error:` (e.g. `… ERROR growwapi…: Error: <e>`) | Error | same | the SDK's own swallowed-error logging |
| `subscribed N stocks + M indices — awaiting first tick…` | Subscribed | (none) | subscribe confirmation |
| `groww auth OK…` / `→ appending NDJSON…` | Streaming | (none) | positive/recovery edge (clears auth_rejected) |
| everything else (`NATS error_cb ->…`, `NATS disconnected ->…`, `FEED STALLED —…`, `watch file unreadable…`, `DROP[reason] sample…`, capture/walker diagnostics) | Info | (none) | tracing-only |

---

## §2. FEED-SUPERVISOR-01 — feed supervisor task respawned

**Severity:** High. **Auto-triage safe:** Yes (the respawn already self-healed).

**Trigger:** a feed sidecar SUPERVISOR task itself died (panic / unexpected
return) and the respawning wrapper (WS-GAP-05 / DISK-WATCHER-01 pattern) caught
it, logged `error!`, incremented `tv_feed_supervisor_respawn_total{feed}`, and
re-spawned it — so the stall-watchdog can never die silently and leave the feed
unsupervised. The feed is briefly unsupervised between death and respawn; the
relaunch loop resumes on respawn.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `FEED-SUPERVISOR-01`; inspect the
   panic backtrace in `data/logs/errors.jsonl.*` immediately preceding it.
2. A one-off respawn at shutdown is benign. A sustained
   `tv_feed_supervisor_respawn_total{feed}` rate means the supervisor keeps
   panicking — a real bug; restart the app to reset from a clean state and file
   the backtrace.

**Source:** `crates/common/src/error_code.rs::ErrorCode::FeedSupervisor01Respawned`,
`crates/app/src/main.rs` (the respawning `tokio::spawn` wrapper),
`crates/app/src/groww_sidecar_supervisor.rs` (`should_respawn_supervisor`),
and — since 2026-07-02 — `crates/app/src/groww_bridge.rs::spawn_supervised_groww_bridge`
(the NDJSON-consumer bridge task, `component="bridge"` on the respawn counter:
a bridge panic used to silently stop ALL Groww persistence while the stall
watchdog killed the wrong process; the supervisor respawns it with 5s→60s
backoff — the re-tail is DEDUP-idempotent and bars survive on the shared
aggregator). Companion fix, same sweep: the stall watchdog's liveness signal is
now PARSE-time (`FeedHealthRegistry::record_feed_liveness`, stamped when NDJSON
lines are parsed, independent of the QuestDB flush) — so a DB outage can no
longer mimic a dead socket and trigger a false FEED-STALL-01 kill of a healthy
sidecar.

---

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `FeedStall01*` / `FeedSupervisor01*` variant)
- `crates/app/src/groww_sidecar_supervisor.rs`
- `crates/common/src/feed_health.rs` (`last_tick_age_secs`)
- Any file containing `FEED-STALL-01`, `FEED-SUPERVISOR-01`, `FEED-REJECT-01`,
  `FeedStall01`, `FeedSupervisor01`, `FeedReject01`, `should_restart_on_stall`,
  `should_restart_on_never_streamed`, `sidecar_line_signature`,
  `tv_feed_sidecar_stall_restart_total`, or
  `tv_feed_sidecar_never_streamed_restart_total`
