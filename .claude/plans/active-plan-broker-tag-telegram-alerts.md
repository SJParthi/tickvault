# Implementation Plan: Broker/feed tag on every broker-specific Telegram alert (🔷 DHAN / 🟢 GROWW)

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator directive 2026-07-14, relayed verbatim via the coordinator session)

> **Operator demand (2026-07-14):** today's Telegram alerts were inconsistent about
> WHICH BROKER/FEED they concern. Every alert whose subject is feed/broker-specific
> must carry an unambiguous broker tag (🔷 DHAN / 🟢 GROWW) in the TITLE; genuinely
> feed-agnostic alerts (host disk, box shutdown, process death) must explicitly read
> as host/system-level. The per-minute REST pull alerts must name the feed AND the
> leg (Dhan spot 1-minute vs Groww option chain, etc.). Recovered/edge-cleared
> messages carry the SAME tag as their trigger.
>
> **Per-item guarantee matrix:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical 15-row +
> 7-row matrices) per that rule's cross-reference clause. Honest 100% claim
> (charter §F wording): 100% inside the tested envelope, with ratcheted
> regression coverage — the badge assignment is a single centralized match
> (`NotificationEvent::feed_badge()`), every new badge arm is pinned by the
> extended badge ratchet tests so a future un-badging fails the build; ≤60s
> QuestDB outage absorbed by rescue→spill→DLQ; ≤100,000-tick ring buffer
> capacity (constant `TICK_BUFFER_CAPACITY`, ratcheted by
> `zero_tick_loss_alert_guard.rs`); bench-gated O(1) hot path (this change is
> COLD-PATH notification formatting only — zero hot-path involvement);
> composite-key uniqueness; chaos-tested 65h weekend sleep/wake. NOT claimed:
> any change to alarm ROUTING, severities, dispatch policies, or delivery —
> wording/badge only. Beyond the envelope, DLQ NDJSON catches every payload as
> recoverable text.

## Plan Items

- [x] Item 1 — Badge the Dhan per-minute REST-leg events + other Dhan-scoped events in the central `feed_badge()` match (audit §2c + §2e)
  - Files: crates/core/src/notification/events.rs
  - Tests: test_dhan_rest_leg_events_carry_dhan_badge, test_dhan_scoped_events_carry_dhan_badge_in_message

- [x] Item 2 — Badge all Groww per-minute REST-leg events (audit §2d)
  - Files: crates/core/src/notification/events.rs
  - Tests: test_groww_rest_leg_events_carry_groww_badge

- [x] Item 3 — Derive the badge from the `feed` field (with Dhan fallback for the legacy "main"/"order_update" values) on WebSocketSleepEntered / WebSocketSleepResumed / WebSocketTokenForceRenewedOnWake
  - Files: crates/core/src/notification/events.rs
  - Tests: test_sleep_events_badge_follows_feed_field_with_dhan_fallback

- [x] Item 4 — Name the LEG in the per-minute REST titles (spot index vs option chain vs option contract), both feeds; reword generic "the broker" to "Dhan" in the Dhan REST-leg bodies
  - Files: crates/core/src/notification/events.rs
  - Tests: test_spot_1m_fetch_degraded_is_high_with_action_lines, test_groww_spot_1m_fetch_degraded_wording

- [x] Item 5 — Episode recovered / stale-closed lines carry the same feed badge as the DOWN bubble (audit finding G7)
  - Files: crates/core/src/notification/episode.rs
  - Tests: test_render_episode_recovered_carries_feed_badge, test_render_episode_stale_closed_carries_feed_badge

- [x] Item 6 — StartupComplete per-minute capture line names DHAN per leg + notes the Groww legs report separately; EndOfDayDigest "Main feed" line says Dhan
  - Files: crates/core/src/notification/events.rs
  - Tests: test_startup_complete_message_contains_context, test_end_of_day_digest_happy_path_message

- [x] Item 7 — Lambda ALARM_PHRASES: DHAN tag on token-remaining-low / order-update-ws-inactive / ws-pool-all-dead / ws-failed-connections / ws-reconnect-gap-high / tick-gap-instruments-silent; HOST tag on market-hours-liveness-missing
  - Files: deploy/aws/lambda/telegram-webhook/handler.py, deploy/aws/lambda/telegram-webhook/test_handler.py
  - Tests: test_handler.py (phrase pins updated)

- [x] Item 8 — Update every Rust test pinning titles/badges + EXTEND the badge ratchets so the new assignments are build-failing on removal
  - Files: crates/core/src/notification/events.rs, crates/core/tests/event_formatting_coverage.rs
  - Tests: test_dhan_rest_leg_events_carry_dhan_badge, test_groww_rest_leg_events_carry_groww_badge, test_non_feed_events_carry_no_feed_badge

## Design

The fix reuses the EXISTING centralized badge mechanism in the `tickvault-core`
crate (crates/core): `NotificationEvent::feed_badge()` (crates/core/src/notification/events.rs)
returns `Some("🔷 DHAN")` / `Some("🟢 GROWW")` per variant and
`NotificationEvent::to_message()` prepends `{badge} — ` to the body, so BOTH
dispatch paths (immediate bypass + coalescer) carry the tag identically with
zero emit-site changes. This plan:

1. Adds the Dhan REST-leg variants (Spot1m*, Chain*), the market-open trio,
   CrossVerify1m*, BarMismatch*, IpVerification*, StaticIpBootCheck*,
   DualInstanceDetected, and the OMS order-path events (OrderRejected,
   CircuitBreaker*, RateLimitExhausted, OrphanPosition*) to the static Dhan
   arm. SCOPE EXCLUSIONS (coordinator 2026-07-14 — a parallel session is
   deleting Dhan machinery): NoLiveTicksDuringMarketHours, RestCanary*,
   the OrderUpdate* events, MidSessionProfileInvalidated +
   TokenForcedRemintTriggered (already badged — arms left byte-identical),
   and episode.rs BootMilestone arms are NOT touched by this plan.
2. Adds all nine `Groww*1m*` variants to the static Groww arm.
3. Converts the three sleep/wake variants that carry a `feed: String` field
   from the static Dhan arm to a dynamic resolve with a Dhan fallback (the
   live field values are "main"/"order_update" — both Dhan WS types; a future
   "groww" value would badge correctly, an unknown value degrades to Dhan
   which is honest for these Dhan-connection-owned events).
4. Rewords the spot-leg titles to name the LEG ("spot index candle pull") so
   Dhan spot vs Dhan chain vs Groww spot/chain/contract are each
   distinguishable even when a client truncates the badge.
5. Episode machinery (crates/core/src/notification/episode.rs): the
   recovered and stale-closed renders prepend `EpisodeFamily::badge()` for
   non-Boot families, matching the DOWN bubble's tag.
6. Host/system convention (documented next to `feed_badge()`): genuinely
   feed-agnostic events stay UN-badged in Rust (their titles already name
   tickvault/QuestDB/the server); the lambda `ALARM_PHRASES` host-ambiguous
   phrase gains an explicit "HOST — " prefix, and broker-scoped alarm phrases
   gain "DHAN — ". Severity emoji stays FIRST on the wire (the prefix layer
   in service.rs / the lambda `_house_line` emoji is untouched).

## Edge Cases

- Recovered/edge-cleared variants are badged in the SAME PR commit as their
  triggers (pairwise: Spot1mFetchDegraded↔Recovered, SidNotServed↔Recovered,
  ChainFetchDegraded↔Recovered, Groww pairs likewise) — a recovery can never
  render un-badged while its trigger is badged.
- Unknown/future `feed` field values on the generic `Feed*` events still
  render UN-badged (never a wrong badge); the three sleep/wake variants
  fall back to DHAN because their emit sites live in the Dhan connection
  code ("main"/"order_update" values).
- The lambda OK-flip line (`✅ Recovered: {phrase} — {IST} IST`) reuses the
  same phrase string, so the DHAN/HOST prefix carries into recoveries
  automatically.
- `ChainEntitlementAbsent` has two bodies (enabled vs probe) — both get the
  badge via the single variant arm.
- Boot episode family keeps its 🚀 badge and is deliberately excluded from
  the recovered/stale-closed feed-badge prepend (not a broker).
- HTML-escaped bodies: the badge is prepended OUTSIDE `message_body()`, so
  escaping behavior is unchanged.

## Failure Modes

- A future variant added to the Dhan/Groww REST families without a badge →
  caught by the extended badge ratchet tests (build-failing).
- A wrong badge on a host event → caught by
  `test_non_feed_events_carry_no_feed_badge` (kept, with the newly-badged
  IpVerificationSuccess swapped out for genuinely host events).
- Lambda phrase drift → `test_handler.py` pins the new phrases; the Rust
  `telegram_lambda_house_style_guard.rs` is structure-only and unaffected.
- No dispatch/severity/routing change anywhere — a formatting regression can
  degrade wording only, never delivery.

## Test Plan

- `cargo fmt --check` + `cargo clippy -p tickvault-core --no-deps -- -D warnings -W clippy::perf`
- `cargo test -p tickvault-core` (full lib + integration: events.rs test
  module incl. the extended badge ratchets, event_formatting_coverage.rs,
  ws_telegram_visibility_guard.rs, mid_session_profile_watchdog_guard.rs,
  episode render tests)
- `python3 -m pytest deploy/aws/lambda/telegram-webhook/test_handler.py`
- New ratchets: `test_dhan_rest_leg_events_carry_dhan_badge`,
  `test_groww_rest_leg_events_carry_groww_badge`,
  `test_sleep_events_badge_follows_feed_field_with_dhan_fallback`,
  `test_render_episode_recovered_carries_feed_badge`,
  `test_render_episode_stale_closed_carries_feed_badge`.

## Rollback

Pure wording/badge change on the cold notification path: `git revert` of the
single commit restores the previous titles byte-for-byte. No schema, no
config, no alarm, no dispatch-policy change — nothing to migrate back. The
lambda change rolls back with the same revert (deployed by the next
terraform/deploy run).

## Observability

This change IS observability (operator-facing alert wording). No new
metrics/alarms are added or removed; severities and dispatch policies are
untouched (severity ratchet tests stay green). The extended badge ratchet
tests are the mechanical guarantee that every broker-scoped alert keeps its
tag; the lambda phrase pins in test_handler.py guarantee the alarm-driven
lines keep theirs. Delivery boundary unchanged: events route via the
existing 5-sink chain; alarm lines via SNS → the Telegram webhook lambda.
