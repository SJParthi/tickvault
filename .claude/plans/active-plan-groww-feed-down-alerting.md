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
   (tickvault-core): `FeedDown { feed, reason, market_open }` — field-driven
   severity: **High in-market / Low off-hours** (precedent: CrossVerify1mSummary
   field-driven severity; WebSocketDisconnected High vs …OffHours Low split) —
   and `FeedRecovered { feed, down_secs }` (**Medium**, mirrors
   WebSocketReconnected). Exhaustive arms: `message_body()`, `topic()`,
   `severity()`; wildcard arms edited: `feed_badge()` (feed-generic
   `feed_badge_for_name` arm). NO `dispatch_policy()` edit (High/Medium bypass
   the coalescer; Low off-hours coalescing 60s is the desired
   no-pre-market-spam behavior). NO `NotificationService` edit (generic
   consumer). NO new ErrorCode (no new `error!` mentions a tracked code).
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
   Handles hoisted ONCE before the loop (zero per-wake allocation, static
   labels): `tv_groww_ws_active` (connected-level 0/1 — 1 while the connected
   episode holds [boot-connect announced OR streaming observed], explicitly 0
   in both falling-edge arms; connected-level chosen so the pre-open
   08:30→first-tick window does not page daily, mirroring
   `tv_order_update_ws_active` semantics) and
   `tv_feed_last_tick_age_seconds{feed="groww"}` from
   `FeedHealthRegistry::last_tick_age_secs` — SKIP the `.set()` on `None`
   (no first tick yet → missing data + `notBreaching` alarm = no false signal;
   NEVER a fake 0 age).
5. CloudWatch: append `tv_groww_ws_active`, `tv_feed_last_tick_age_seconds`,
   `tv_feed_sidecar_stall_restart_total` to BOTH selector copies
   (`deploy/aws/cloudwatch-agent.json` + `deploy/aws/terraform/user-data.sh.tftpl`
   — byte-identical, lockstep-ratcheted). Two new terraform alarms in
   `app-alarms.tf`: `tv-${var.environment}-groww-ws-inactive` (exact clone of
   `order_update_ws_inactive`: Minimum < 1, period 60, eval 2, notBreaching)
   and `tv-${var.environment}-groww-stall-restart-storm` on
   `tv_feed_sidecar_stall_restart_total` (Maximum >= 3, period 300, eval 1,
   notBreaching — HOUSE-PRECEDENT shape: plain statistic on the session-scoped
   cumulative counter exactly like `late_tick_after_boundary`; the box restarts
   daily at 08:30 IST so the counter is fresh each session; "3+ stall restarts
   since today's boot" is the flapping-socket / provider-reject page per
   FEED-STALL-01 storm semantics [in-process storm = >5 restarts / 300s], while
   a single self-healing restart never pages). Both registered in the
   `app_cloudwatch_alarms` output list. Dhan files untouched.

## Edge Cases

- Notifier slot unfilled at a falling edge (boot ordering): audit row still
  written, page skipped, latch still set — bounded fail-open, documented; the
  rising edge DEFERS (doesn't consume the latch) when the slot is unfilled.
- Off-hours disable → `FeedDown` Low, 60s-coalesced (no pre-market spam —
  the 2026-04-22 WebSocketDisconnectedOffHours precedent class).
- Pre-open 08:30→first-tick: connected-level `tv_groww_ws_active` reads 1 once
  the sidecar reports subscribed (no daily false page); the age gauge is
  UNPUBLISHED until the first tick (missing + notBreaching = silent).
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
- Cumulative-counter alarm mis-shape: mirrored from the VERIFIED in-repo
  precedent (`late_tick_after_boundary` Maximum-statistic on a session-scoped
  counter); the alarm description states the honest "since today's boot"
  semantics, never claiming a 5-minute delta.
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
  `test_feed_down_and_recovered_resolve_groww_badge`.
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
- Metrics: `tv_groww_ws_active` (gauge 0/1, connected-level),
  `tv_feed_last_tick_age_seconds{feed="groww"}` (gauge, absent pre-first-tick),
  existing `tv_feed_sidecar_stall_restart_total` now CW-exported.
- CloudWatch alarms: `tv-${env}-groww-ws-inactive` (Minimum < 1, 60s × 2,
  notBreaching) and `tv-${env}-groww-stall-restart-storm` (Maximum >= 3 over
  300s, notBreaching, session-scoped counter) → SNS `tv_alerts` → Telegram.
- Audit: existing `ws_event_audit` rows unchanged (Connected / Disconnected /
  DisconnectedOffHours / Reconnected, feed='groww').
- Logs: existing FEED-STALL-01 / FEED-SUPERVISOR-01 `error!` sites unchanged;
  no new ErrorCode.
