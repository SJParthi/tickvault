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

**2026-07-06:** the storm signal now pages via the
`tv-<env>-errcode-feed-stall-01` log-filter alarm (Sum ≥ 3 per 15 min → SNS →
Telegram, `deploy/aws/terraform/error-code-alarms.tf`); a single benign
self-heal restart stays page-free by design (this section's own
operator-action bound). Before 2026-07-06 the `error!` reached only the log
sinks — the error!→Telegram route was severed by the CloudWatch-only
migration.

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
- Any file containing `FEED-STALL-01`, `FEED-SUPERVISOR-01`, `FeedStall01`,
  `FeedSupervisor01`, `should_restart_on_stall`, or `tv_feed_sidecar_stall_restart_total`
