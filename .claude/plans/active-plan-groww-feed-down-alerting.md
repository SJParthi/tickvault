# Implementation Plan: Groww Feed-Down Alerting (Telegram event + CloudWatch gauges/alarms)

**Status:** APPROVED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06, ultracode session)
**Branch:** `claude/charming-hamilton-eo3rhf`
**Changed crates:** `tickvault-core` (crates/core), `tickvault-app` (crates/app), `tickvault-storage` (crates/storage — test-only ratchet), plus `deploy/aws/`

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row, mandatory
> per `per-item-guarantee-check.sh`). Dominant guarantee here: **strictly no
> worse than today** — purely additive alerting; no hot-path (per-tick) code
> touched (gauge handles hoisted once before the bridge loop, cold ≤1s-cadence
> writes); no Dhan file edited; no schema change; Groww stays LIVE-FEED-ONLY
> (zero REST/historical calls added); no token path touched.

## Plan Items

- [x] Item 1 — Typed `FeedDown` / `FeedRecovered` NotificationEvents (edge-latched, honest recovery)
  - Files: crates/core/src/notification/events.rs, crates/core/tests/event_formatting_coverage.rs
  - Tests: test_feed_down_in_market_is_high_severity, test_feed_down_off_hours_is_low_severity, test_feed_recovered_is_medium_severity, test_feed_down_renders_reason_and_topic_and_severity, test_feed_down_html_escapes_reason, test_feed_recovered_renders_down_secs_and_topic, test_feed_down_and_recovered_resolve_groww_badge, test_feed_down_and_recovered_message_coverage

- [x] Item 2 — Emit sites + per-episode DOWN latch in the Groww bridge (feed-disable falling edge + bridge-death falling edge + streaming-rising-edge recovery)
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_try_announce_feed_down_fires_once_per_episode, test_take_feed_recovery_returns_down_secs_and_rearms, test_take_feed_recovery_none_when_not_down, crates/app/tests/groww_feed_down_alert_guard.rs (source-scan ratchets)

- [x] Item 3 — Groww liveness gauges published from the bridge loop
  - Files: crates/app/src/groww_bridge.rs
  - Tests: groww_feed_down_alert_guard.rs::test_gauges_published_from_bridge_loop (source-scan)

- [x] Item 4 — CloudWatch selectors + terraform alarms + lockstep ratchet
  - Files: deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/terraform/app-alarms.tf, crates/storage/tests/cw_agent_selector_lockstep_guard.rs
  - Tests: cw_agent_selector_lockstep_guard.rs (selector lockstep + 3 metric names + 2 alarm resources + composite-list registration)

## Design

Groww feed-DOWN transitions today write only `ws_event_audit` rows + logs — no
Telegram (`notifier.notify` appears exactly once in groww_bridge.rs, on the
boot-connect rising edge at ~2374). Fix (crates: **tickvault-core** +
**tickvault-app**, ratchet in **tickvault-storage**):

1. Two new `NotificationEvent` variants in `crates/core/src/notification/events.rs`
   (tickvault-core): `FeedDown { feed, reason, market_open, operator_initiated }`
   — field-driven severity: **High in-market / Low off-hours** (precedent:
   CrossVerify1mSummary field-driven severity; WebSocketDisconnected High vs
   …OffHours Low split). `market_open` is the CALENDAR-aware
   `is_trading_session_now()` (never the clock-only helper — Saturday/holiday
   false-page class, 2026-05-09 precedent; ratcheted in the guard).
   `operator_initiated` branches the body trailer: a deliberate feeds-page
   disable says "stays off until you re-enable it", a bridge death says the
   honest "retrying automatically" — and `FeedRecovered { feed, down_secs }`
   (**Medium**, mirrors WebSocketReconnected). Exhaustive arms:
   `message_body()`, `topic()`, `severity()`; wildcard arms edited:
   `feed_badge()` (feed-generic `feed_badge_for_name` arm). NO
   `dispatch_policy()` edit (High/Medium bypass the coalescer; Low off-hours
   coalescing 60s is the desired no-pre-market-spam behavior). NO
   `NotificationService` edit (generic consumer). NO new ErrorCode (no new
   `error!` mentions a tracked code).
2. Emit sites in `crates/app/src/groww_bridge.rs` (tickvault-app), once per
   DOWN episode via two new `GrowwAuditLatches` fields — `down_announced:
   AtomicBool` + `down_since_epoch_secs: AtomicU64` (first-down-wins CAS so
   `down_secs` spans intermediate retry failures, mirroring the Dhan
   `record_disconnect` total-downtime semantics):
   (a) the feed-disable falling-edge arm (the `!feed_runtime.is_enabled` block,
   inside the existing `audited_connected` gate next to the `"feed_disabled"`
   audit row) and (b) the bridge-death falling edge in
   `supervise_groww_bridge_loop` (inside the existing `audited_connected.swap`
   gate next to the `"bridge_died"` audit row). Both read the SAME lazily-filled
   `notifier_slot` the boot-connect arm reads. Fail-open on an unfilled slot at
   a falling edge: audit row still written, latch still set, page skipped
   (never blocks the disable/respawn path; bounded to boot-ordering).
3. Recovery: on the streaming rising edge (the `"groww_resumed"` /
   `"groww_sidecar"` classify block), if a DOWN episode is latched AND the
   notifier is deliverable, `take_feed_recovery` consumes the latch and fires
   ONE `FeedRecovered { down_secs }`. Recovery is claimed on STREAMING (ticks /
   sidecar `streaming` status), never socket-connect — the boot-connect
   `FeedConnectedAwaitingTicks` ("awaiting first tick") is UNCHANGED, keeping
   the connected≠streaming honesty split. If the slot exists but is unfilled,
   the rising edge DEFERS (does not consume the latch) to the next ≤1s wake.
4. Gauges (tickvault-app), published from the `run_groww_bridge` loop (the pool
   watchdog is Dhan-lane-spawned and never runs on a groww-only boot; the
   bridge loop wakes ≤1s and already writes `feed_health.set_connected`).
   Handles are registered LAZILY (`Option::get_or_insert_with`) — NOT hoisted
   before the loop, and deliberately NOT an exact clone of the order-update
   registration site, though the SEMANTICS mirror the order-update precedent:
   `tv_groww_ws_active` (connected-level 0/1 — 1 while the connected episode
   holds [boot-connect announced OR streaming observed]) is REGISTERED only at
   the first connected episode of the session, so the metric stays MISSING
   (notBreaching → alarm silent) through a boot/activation chain of ANY length
   — the round-2 fix for the unverified "~2min grace" that false-paged every
   slow boot morning. Once registered it publishes 0/1 every wake (and 0 on
   disabled wakes), so mid-session outages, failed disable→re-enables, and
   runtime disables all show. Honest envelope: a sidecar dead at boot leaves
   the metric missing — that class pages via the sidecar reject alerts +
   FEED-STALL-01, not this alarm. `tv_feed_last_tick_age_seconds{feed="groww"}`
   from `FeedHealthRegistry::last_tick_age_secs` — SKIP the `.set()` on `None`
   (no first tick yet → missing data + `notBreaching` alarm = no false signal;
   NEVER a fake 0 age).
4b. Evidence freshness (round-2 hardening, 2026-07-06): BOTH streaming-evidence
   channels are floored so a fossil can never latch the connected episode,
   fire FeedRecovered, or pin the gauge at 1 —
   (a) STATUS channel: `groww_status_is_live(stamp, now, floor)` — stamp
   present + at/after the live floor (bridge-incarnation start, advanced every
   disabled wake) + within 120s of now + not absurdly future. The one-shot
   `subscribed` record's freshness LATCHES per episode
   (`fresh_status_seen_this_episode`) so the boot-connect notifier deferral
   stays "delayed, never lost" on >120s boot chains. EPOCH CONTRACT: the
   sidecar's `now_ist_nanos()` stamp is IST-epoch (UTC+19,800s, the
   `replace(tzinfo=timezone.utc)` trick shared with `ms_to_ist_nanos`),
   matching the bridge's `receipt_ist_nanos()` clock — cross-language ratchet
   `test_sidecar_status_stamp_shares_the_ist_epoch_convention` (the round-1
   gate compared a UTC stamp to an IST clock: every real record read 19,800s
   stale forever).
   (b) TICK channel: `parsed_a_tick` latches streaming ONLY when
   `wake_had_fresh_capture(floor)` — the drained lines' per-message
   `capture_ns` stamps prove capture AFTER the floor. The ≤2s pre-disable
   NDJSON backlog replayed on re-enable, and a respawned bridge's byte-0
   re-tail, still persist + fold (zero loss) but prove nothing about liveness.
5. CloudWatch: append `tv_groww_ws_active`, `tv_feed_last_tick_age_seconds`,
   `tv_feed_sidecar_stall_restart_total` to BOTH selector copies
   (`deploy/aws/cloudwatch-agent.json` + `deploy/aws/terraform/user-data.sh.tftpl`
   — byte-identical, lockstep-ratcheted). Two new terraform alarms in
   `app-alarms.tf`: `tv-${var.environment}-groww-ws-inactive` (Minimum < 1,
   period 60, eval 2, notBreaching — same SHAPE as `order_update_ws_inactive`;
   registration semantics per item 4) and
   `tv-${var.environment}-groww-stall-restart-storm` on
   `tv_feed_sidecar_stall_restart_total` (**Sum >= 3 over one 3600s period**,
   notBreaching). SHAPE HONESTY (corrected during implementation, ratcheted by
   `cw_agent_selector_lockstep_guard.rs`): the CW agent's Prometheus pipeline
   DELTA-CONVERTS counter-type metrics — each datapoint is the increase since
   the previous 60s scrape, never the cumulative session count — so the
   originally-planned "Maximum >= 3 / 300s on the session-scoped counter"
   (modeled on the `late_tick_after_boundary` precedent) could NEVER fire (a
   behaviorally dead alarm; that precedent is itself suspect under the same
   delta analysis and is tracked separately). Sum over a 1-hour window counts
   restarts within the window under delta semantics: 3+ stall restarts/hour
   pages, a single self-healing restart stays silent per FEED-STALL-01. Both
   alarms registered in the `app_cloudwatch_alarms` output list. Dhan files
   untouched.

## Edge Cases

- Notifier slot unfilled at a falling edge (boot ordering): audit row still
  written, page skipped, latch still set — bounded fail-open, documented; the
  rising edge DEFERS (doesn't consume the latch) when the slot is unfilled.
- Off-hours disable → `FeedDown` Low, 60s-coalesced (no pre-market spam —
  the 2026-04-22 WebSocketDisconnectedOffHours precedent class).
- Pre-open 08:30→first-tick: connected-level `tv_groww_ws_active` reads 1 once
  the sidecar reports a FRESH subscribed status (no daily false page); before
  the first connected episode of the session the gauge is UNREGISTERED
  (missing + notBreaching = silent for a boot chain of any length — round-2
  fix); the age gauge is UNPUBLISHED until the first tick.
- Disable→re-enable with a failing relaunch (auth reject / stale-token class):
  the pre-disable frozen status file AND the ≤2s pre-disable NDJSON backlog
  are BOTH below the live floor — no false FeedRecovered, no consumed DOWN
  episode, gauge publishes 0 (registered earlier in the session) so the
  groww-ws-inactive alarm fires honestly (round-2 fix — the round-1 gate
  covered only the status channel).
- Tickless window (closed-market run / pre-open) with a slow notifier fill:
  the one-shot `subscribed` record ages past 120s, but the per-episode
  freshness latch keeps the boot-connect deferral open — the ping is delayed,
  never lost (round-2 fix).
- One-off stall restart that self-heals: NO Telegram FeedDown (the stall path
  emits no typed event; FEED-STALL-01 stays `warn!`/`error!` per its runbook);
  the storm signal pages via the new CloudWatch alarm on the counter instead.
- Repeated bridge deaths within one episode: latch fires FeedDown once;
  `down_secs` spans the whole episode (first-down-wins CAS).
- Box auto-stop 16:30 IST / weekends / deploy gap: missing metric +
  `notBreaching` = silent (the 2026-06-02 stuck-FIRING fix rationale).
- Feed disabled while never connected: no audit row today, and no FeedDown
  either (both stay inside the `audited_connected` gate) — a feed that was
  never up produces no down page.
- `reason` is always a fixed plain-English literal (never child stderr / URLs)
  + `html_escape` defense-in-depth at the render boundary.
- Fleet (scale-lab, local-only, AWS-locked-out) bridges each carry their own
  latches: a fleet disable would page per-connection — accepted; the fleet is
  dormant on main per `groww-scale-aws-lockout-2026-07-06.md`.

## Failure Modes

- QuestDB/audit consumer down: the Telegram path is independent (notify is
  in-process) — the page still fires; the audit-row drop is already loud
  (AUDIT-WS-01).
- Telegram send failure: existing `send_telegram_chunk_with_retry` +
  TELEGRAM-01 drop accounting — no new handling.
- CW-agent selector drift between the two copies: build-failing lockstep guard
  (`crates/storage/tests/cw_agent_selector_lockstep_guard.rs`).
- Cumulative-counter alarm mis-shape: the CW agent DELTA-CONVERTS Prometheus
  counters, so a plain-Maximum threshold on a "session-scoped cumulative"
  counter can never fire (the `late_tick_after_boundary` precedent this plan
  originally mirrored is itself suspect under the same analysis). Shipped
  shape: Sum >= 3 over 3600s (delta semantics — restarts within the hour);
  pinned by `cw_agent_selector_lockstep_guard.rs`, which rejects a regression
  to Maximum.
- Stale/replayed sidecar evidence (round-2): the status file is never deleted
  and the NDJSON backlog replays on re-enable/respawn — both channels are
  floored (`groww_status_is_live` / `wake_had_fresh_capture`) so fossils
  persist+fold but never latch streaming, fire FeedRecovered, or pin the
  gauge; a cross-language epoch ratchet pins the stamp convention.
- Latch stuck true after a lost rising edge (process restart mid-episode):
  latches are per-process — a restart resets them; worst case one duplicate
  FeedDown after restart (idempotent for the operator, no false-OK).
- `feed` label on the age gauge may not survive as a CW dimension (agent
  dimensions are `[["host"]]`): diagnostic-only metric, no alarm depends on
  it — non-blocking; flagged for live verification.

## Test Plan

- `crates/core/src/notification/events.rs` inline:
  `test_feed_down_in_market_is_high_severity`,
  `test_feed_down_off_hours_is_low_severity`,
  `test_feed_recovered_is_medium_severity`,
  `test_feed_down_renders_reason_and_topic_and_severity` (commandments: emoji,
  feed name, no `.rs`/lib jargon, topic, severity),
  `test_feed_down_html_escapes_reason`,
  `test_feed_recovered_renders_down_secs_and_topic`,
  `test_feed_down_and_recovered_resolve_groww_badge`,
  `test_feed_down_operator_disable_body_names_the_action_not_auto_retry`
  (the `operator_initiated` trailer honesty).
- Round-2 additions in `crates/app/src/groww_bridge.rs` inline:
  `test_sidecar_status_stamp_shares_the_ist_epoch_convention` (cross-language
  epoch ratchet), `test_capture_stamp_ist_nanos_trusted_stamp_only_no_fallback`,
  `test_wake_had_fresh_capture_gates_on_floor_and_zero`,
  `test_drain_replayed_backlog_parses_but_never_reads_fresh` (end-to-end
  through the real drain body), plus the guard ratchets
  `test_tick_channel_streaming_latch_is_freshness_gated` and the
  `is_trading_session_now` / episode-latch / floor-rename pins in
  `groww_feed_down_alert_guard.rs`.
- `crates/core/tests/event_formatting_coverage.rs`: render cases for both
  variants (`test_feed_down_and_recovered_message_coverage`).
- `crates/app/src/groww_bridge.rs` inline: latch unit tests
  (`test_try_announce_feed_down_fires_once_per_episode`,
  `test_take_feed_recovery_returns_down_secs_and_rearms`,
  `test_take_feed_recovery_none_when_not_down`).
- NEW `crates/app/tests/groww_feed_down_alert_guard.rs` (house Pattern B,
  comment-stripped production-region window scans, `markers >= 1` anti-vacuous
  floors): (a) the `"feed_disabled"` arm emits `NotificationEvent::FeedDown`;
  (b) the `"bridge_died"` arm too; (c) `FeedRecovered` + `take_feed_recovery`
  in the rising-edge region; (d) both gauge literals published in
  groww_bridge.rs with a static `"feed" => "groww"` label; (e) Dhan
  blast-radius pin: `../core/src/websocket/connection.rs` contains NO
  `FeedDown`; (f) cw-agent config carries both gauge names.
- NEW `crates/storage/tests/cw_agent_selector_lockstep_guard.rs`: both selector
  files contain the 3 new metric names; the two `metric_selectors` regex
  strings are byte-identical; `app-alarms.tf` contains both alarm names +
  their `app_cloudwatch_alarms` output registrations.
- Scoped runs: `cargo test -p tickvault-core`, `-p tickvault-app`,
  `-p tickvault-storage` (new guard), `cargo fmt --check`, scoped clippy,
  banned-pattern scanner.

## Rollback

Revert the branch's commits — variants + emit sites + gauges are additive; no
schema change, no config-default change, no Dhan surface. Reverting the deploy
commit restores the selector lists and destroys the 2 alarms on the next
terraform apply; the storage guard reverts with it. No data migration either
direction; feed behavior (connect/reconnect/capture/persist) is unchanged —
this branch is alerting-only.

## Observability

- Telegram: `FeedDown` (High in-market → immediate + SNS; Low off-hours →
  60s-coalesced), `FeedRecovered` (Medium → immediate), both feed-badged
  🟢 GROWW via `feed_badge_for_name`.
- Metrics: `tv_groww_ws_active` (gauge 0/1, connected-level, registered at the
  first connected episode of the session — missing through boot),
  `tv_feed_last_tick_age_seconds{feed="groww"}` (gauge, absent pre-first-tick),
  existing `tv_feed_sidecar_stall_restart_total` now CW-exported.
- CloudWatch alarms: `tv-${env}-groww-ws-inactive` (Minimum < 1, 60s × 2,
  notBreaching) and `tv-${env}-groww-stall-restart-storm` (Sum >= 3 over one
  3600s period, notBreaching — delta-converted counter semantics; see Design
  item 5) → SNS `tv_alerts` → Telegram.
- Audit: existing `ws_event_audit` rows unchanged (Connected / Disconnected /
  DisconnectedOffHours / Reconnected, feed='groww').
- Logs: existing FEED-STALL-01 / FEED-SUPERVISOR-01 `error!` sites unchanged;
  no new ErrorCode.
