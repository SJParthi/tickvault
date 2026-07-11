# Implementation Plan: Groww Boot-Visibility Parity (boot-stage Telegram + audit signals)

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — verbatim demand 2026-07-04: "as per time
date now market hours how the fuck it is showing and displaying the fucking
dhan — i need the same view display everything even for groww"
Evidence: on the local Saturday boot Dhan emitted Telegram at every boot stage
(Auth OK, instrument load, boot complete, order-update WS connected) while
Groww — connected with 768 subscriptions, visible on the /feeds panel — emitted
NOTHING. Cause: the Groww lifecycle events (Telegram + ws_event_audit
feed='groww') are edge-latched on the FIRST STREAMING TICK
(`crates/app/src/groww_bridge.rs` — the `audited_connected` rising edge), so a
closed market = total silence.

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`.
> Per-item specifics are in the Test Plan + Observability sections below.
> Scope is notification/boot COLD paths only — no hot-path change, no QuestDB
> schema change (the feed-keyed `ws_event_audit` table is reused as-is), no
> WebSocket scope change (2-WS Dhan lock + Groww sidecar untouched), no new
> ErrorCode (a failed audit write is still AUDIT-WS-01 on the shared consumer).

## Design

Emit Groww boot-stage signals at the SAME lifecycle moments Dhan gets,
feed-agnostically, wired off the SAME state transitions the /feeds panel uses
(the sidecar PROOF status file `data/groww/groww-status.json` → the
`FeedHealthRegistry` writes in `run_groww_bridge`, and the activation
watcher's auth smoke-check in `groww_activation.rs`):

1. **Auth OK** (`crates/app/src/groww_activation.rs`): the auth smoke-check
   `Ok(())` arm (SSM token read + shape-accepted) emits a new
   `NotificationEvent::FeedAuthOk { feed }` — Severity::Low, immediate
   dispatch — mirroring Dhan's `AuthenticationSuccess` ("Auth OK") ping.
   Delivered via a new bounded `notify_when_ready` helper that polls the
   lazily-filled notifier slot (boot-ordering race: the slot fills only after
   the NotificationService boots) so the ping is never silently lost NOR
   blocks activation. The existing `FeedInstrumentsLoaded` emit moves onto the
   same helper (same race, same fix).
2. **Socket connected + subscribe complete** (`crates/app/src/groww_bridge.rs`):
   on the FIRST observed `subscribed`/`streaming` status per connected episode
   (the same status-file read that already drives `set_subscribed` for the
   /feeds panel), emit — edge-latched via a new process-lifetime
   `GrowwAuditLatches::boot_connect_announced` latch —
   (a) a `ws_event_audit` `feed='groww'` `Connected` row (source
   `groww_subscribed`) AT SOCKET-CONNECT time, and (b) a new
   `NotificationEvent::FeedConnectedAwaitingTicks { feed, subscribed,
   market_open }` Telegram: "Groww feed connected — subscribed N — awaiting
   first tick (market closed — idle is normal)". The EXISTING
   first-streaming-tick rising-edge event (Connected/Reconnected, reason
   "groww streaming") is KEPT UNCHANGED as the streaming confirmation — the
   boot-time event is ADDITIVE. The latch re-arms ONLY on a genuine
   disconnect falling edge (feed disable, bridge death) — never per poll
   turn, never per reconnect attempt.
3. **Boot complete counts both feeds** (`crates/app/src/main.rs` +
   `events.rs`): `WebSocketPoolDeferredOffHours` (the Saturday/off-hours
   "Boot complete" message) gains the same `feeds: Vec<FeedStatusLine>`
   payload `WebSocketPoolOnline` already carries, built from the same
   `build_feed_status_lines` registry read — so an off-hours boot complete
   names EVERY enabled feed instead of only the Dhan connection count.

The notifier handle threads into the bridge as
`Option<Arc<ArcSwapOption<NotificationService>>>` (the same lazily-filled
slot the sidecar supervisor + activation watcher already share), threaded
through `spawn_supervised_groww_bridge` / `spawn_supervised_groww_shard_bridges`
/ `supervise_groww_bridge_loop` / `run_groww_bridge`. `None` (tests) = no-op.

## Edge Cases

- **Market open vs closed at connect time:** the message suffix flips —
  closed → "(market closed — idle is normal)", open → "(market open — ticks
  should arrive shortly)". The text NEVER claims ticks/streaming (false-OK
  trap): socket-connected ≠ streaming; the streaming rising-edge event stays
  the only streaming claim.
- **Poll turns:** the status file is re-read every wake (≤1s cadence); the
  `boot_connect_announced` latch guarantees exactly ONE announcement per
  connected episode — never per-loop spam.
- **Bridge respawn (panic):** the supervisor already emits the Disconnected
  row; it now also re-arms the boot latch, so the respawned bridge announces
  the (re)connected socket once, then the streaming edge classifies
  Reconnected — the forensic chain stays Disconnected-separated.
- **Feed disable → re-enable:** the disable falling edge re-arms the latch
  (genuine reconnect re-announces once); reconnect ATTEMPTS never re-fire.
- **Notifier slot not yet filled (boot ordering):** the bridge skips the
  latch consumption and retries next wake (the announcement is delayed, not
  lost); the activation-side pings use the bounded `notify_when_ready` poll.
- **Old sidecar status file with `Unknown` event tag:** treated as
  not-yet-subscribed (no announcement) — fail-soft, forward-compatible.
- **Scale mode (§34 shard fleet):** each shard conn carries its own latches —
  one announcement per shard connection episode (default OFF; single-conn
  path byte-identical apart from the new signals).
- **Dry-run isolation:** Telegram lifecycle messages are NOT dry-run-gated
  (Dhan's aren't either); only master-table rows carry dry-run isolation —
  unchanged here (no master-table write in this PR).

## Failure Modes

- **ws_event_audit write fails (ILP down):** best-effort `try_send` through
  the SAME shared consumer — a failure is the existing AUDIT-WS-01 Medium on
  the consumer; the bridge loop is never stalled (unchanged contract).
- **Telegram send fails:** NotificationService's own retry/coalescer handles
  it (TELEGRAM-01 on drop); the bridge/activation never blocks.
- **Notifier slot never fills (notification boot failed):** activation pings
  give up after the bounded poll window (logged at info!); the bridge
  announcement waits — no crash, no spam, and the structured `info!` log +
  feed_health counts still record the state.
- **Sidecar never writes the status file:** no announcement fires (nothing to
  announce — honest silence, the /feeds panel shows the same), and the
  existing GrowwSidecarRejected / FEED-STALL-01 chains own the failure.

## Test Plan

- `crates/core/src/notification/events.rs` unit tests: `FeedAuthOk` +
  `FeedConnectedAwaitingTicks` message text (must contain "awaiting first
  tick"; closed-market wording; must NOT claim streaming), topic, severity
  (Low), immediate dispatch policy; `WebSocketPoolDeferredOffHours` renders
  the per-feed block.
- `crates/app/src/groww_bridge.rs` unit tests: `boot_connect_announced` latch
  fires once (not per poll), re-arms on disconnect falling edge and fires
  again after a genuine reconnect; `should_announce_boot_connect` pure
  decision (unknown status / already announced / notifier-not-ready → false);
  `build_groww_ws_audit_row(WsEventKind::Connected, "groww_subscribed", …)`
  row kind/feed/ws_type correctness.
- `crates/app/src/groww_activation.rs` unit tests: `notify_when_ready`
  delivers once the slot fills, gives up after the bounded poll budget.
- Scoped validation: `cargo test -p tickvault-core -p tickvault-app`,
  `cargo fmt --check`, CI clippy command, banned-pattern + pub-fn guards.

## Rollback

Revert the single commit; the Groww lane returns to streaming-edge-only
signals, the DeferredOffHours message drops its feed block, and the two new
event variants disappear. No schema change (ws_event_audit reused as-is), no
config, no state migration — takes effect / rolls back at the next restart.

## Observability

- The change IS observability: Groww gains the Dhan-parity boot-stage view —
  Auth OK ping, connected+subscribed ping (honest "awaiting first tick"
  wording), a boot-time `ws_event_audit` Connected row (queryable forensic
  record), and a feed-aware off-hours boot-complete message.
- Honest envelope: socket-connected ≠ streaming — the boot-time texts keep
  saying "awaiting first tick"; the streaming rising-edge event remains the
  only streaming claim (groww-second-feed rule §5 honest-envelope wording
  holds).
- No new polling loop: all emissions ride the EXISTING bridge wake loop +
  activation task (single source of truth = the same status-file /
  feed-health transitions the /feeds panel reads).
- No new ErrorCode; audit-write failure stays AUDIT-WS-01. Rule file
  `ws-event-audit-error-codes.md` gains a dated 2026-07-04 note for the
  boot-time Connected row.
- Telegram commandments kept: plain English, severity emoji first, specific
  numbers, no file paths/library names; the 💻/☁️ source badge from #1398
  comes free via `telegram_message_prefix`.

## Plan Items

- [x] 1. New Telegram variants + feed-aware off-hours boot complete
  - Files: crates/core/src/notification/events.rs
  - Tests: test_feed_auth_ok_message_topic_severity,
    test_feed_connected_awaiting_ticks_market_closed_wording,
    test_feed_connected_awaiting_ticks_market_open_wording_never_claims_streaming,
    test_pool_deferred_off_hours_lists_enabled_feeds
- [x] 2. Groww Auth OK ping + notifier-ready bounded delivery
  - Files: crates/app/src/groww_activation.rs
  - Tests: test_notify_when_ready_delivers_after_slot_fills,
    test_notify_when_ready_gives_up_after_budget
- [x] 3. Boot-time socket-connected announcement (Telegram + audit row), edge-latched
  - Files: crates/app/src/groww_bridge.rs, crates/app/src/main.rs,
    crates/app/tests/groww_live_pipeline_e2e.rs
  - Tests: test_try_announce_boot_connect_latch_fires_once_not_per_poll,
    test_rearm_boot_connect_latch_rearms_after_genuine_disconnect,
    test_should_announce_boot_connect_decision_table,
    test_boot_connect_audit_row_is_connected_kind_groww_feed
- [x] 4. Rule-file dated note (boot-time Connected row, honest envelope holds)
  - Files: .claude/rules/project/ws-event-audit-error-codes.md
  - Tests: N/A — docs (cross-ref test already green: no new ErrorCode)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Saturday boot, Groww enabled, sidecar subscribes 768 | Telegram: Groww Auth OK → "Groww feed connected — subscribed 768 — awaiting first tick (market closed — idle is normal)"; ws_event_audit Connected row (source groww_subscribed); boot-complete message names Groww |
| 2 | Same boot, poll loop keeps re-reading the status file | Exactly ONE announcement — latch blocks per-poll spam |
| 3 | Market opens, first tick parses | Existing streaming rising-edge event fires unchanged (Connected/Reconnected, "groww streaming") |
| 4 | Feed disabled then re-enabled | Disconnected row on disable; ONE fresh announcement on the next subscribed status (genuine reconnect) |
| 5 | Bridge panics + respawns | Supervisor Disconnected row; ONE fresh announcement; streaming edge classifies Reconnected |
| 6 | Notifier slot empty at announce time | Announcement retried next wake (delayed, never lost); activation pings bounded-poll |
| 7 | Trading-day boot with both feeds streaming | WebSocketPoolOnline unchanged (already feed-aware) |
