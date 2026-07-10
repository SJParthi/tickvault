# Implementation Plan: Feed-Agnostic Telegram Lifecycle Messages (Groww parity)

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — verbatim demand 2026-07-03: "whatever we
have provided for dhan the same should be provided for groww also in Telegram —
instruments load message and other aspects — otherwise how will I know?"
Evidence: today's live "TickVault is live and ready to trade" readiness message
said "All 1 of 1 market-data feeds connected / Feed 1: tracking 776 instruments"
(Dhan-only) while Groww was running with 768 subscribed and ticks flowing; the
15:31:30 End-of-day digest said "Main feed: 1/1" (also Dhan-only).

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`.
> Per-item specifics are in the Test Plan + Observability sections below.
> Scope is notification/boot cold paths only — no hot-path change, no QuestDB
> schema change, no WebSocket scope change (2-WS lock untouched), no new
> ErrorCode (no new `error!` emit site).

## Plan Items

- [x] 1. `FeedStatusLine` + per-feed block in the readiness + EOD Telegram messages
  - Files: crates/core/src/notification/events.rs
  - Tests: test_feed_status_block_renders_known_values,
    test_feed_status_block_renders_unknowns_honestly,
    test_pool_online_message_lists_every_enabled_feed,
    test_end_of_day_digest_lists_every_enabled_feed
  - `WebSocketPoolOnline` + `EndOfDayDigest` gain a `feeds: Vec<FeedStatusLine>`
    payload (feed display name + subscribed instrument count + last-tick age).
    The renderer prints one line per enabled feed; a feed with no known data
    prints "instruments unknown" / "no tick seen yet" — never fake numbers.
    The pool-online header + inner block are relabeled to say "Dhan" so the
    word "feeds" now means market-data feeds (Dhan, Groww, future), not Dhan
    pool connections.
- [x] 2. New `FeedInstrumentsLoaded` Info event (Groww instruments-load parity)
  - Files: crates/core/src/notification/events.rs, crates/app/src/groww_activation.rs, crates/app/src/main.rs
  - Tests: test_feed_instruments_loaded_message_and_severity,
    test_feed_instruments_loaded_skipped_zero_wording
  - Mirrors Dhan's `InstrumentBuildSuccess`: fires once when the Groww
    watch-set resolves at boot/activation with subscribed / indices / stocks /
    skipped counts. Emitted via the existing sidecar notifier slot
    (`ArcSwapOption<NotificationService>`) so activation order is safe.
- [x] 3. Emit-site wiring — build per-feed lines from the live registries
  - Files: crates/app/src/main.rs
  - Tests: test_build_feed_status_lines_includes_enabled_feeds_only,
    test_build_feed_status_lines_honest_unknowns
  - `emit_websocket_connected_alerts` (both boot paths) + the EOD digest task
    take `Arc<FeedRuntimeState>` + `Arc<FeedHealthRegistry>` and build
    `Vec<FeedStatusLine>` at fire time from `FeedHealthRegistry::snapshot`
    (the same source of truth as the portal `/api/feeds/health` endpoint).
    Dhan's subscribed counts are recorded once at boot via
    `feed_health.set_subscribed(Feed::Dhan, stocks, indices)` from the
    subscription-plan summary (cold path, both boot paths); Dhan's last-tick
    age falls back to the global real-tick gap detector when the registry has
    no Dhan tick recorded (honest real-tick signal, never pings).
- [x] 4. Ratchet-test updates for the extended payloads
  - Files: crates/app/tests/ws_pool_online_truthful_emit_guard.rs,
    crates/core/tests/event_formatting_coverage.rs
  - Tests: existing pinned tests updated to construct the new `feeds` field and
    assert the new per-feed block renders.

## Design

One new plain struct in `events.rs`:

```rust
pub struct FeedStatusLine {
    pub name: String,                    // Feed::display_name()
    pub instruments: Option<u64>,        // subscribed stocks+indices; None = unknown
    pub last_tick_age_secs: Option<u64>, // None = no tick seen yet
}
```

A private renderer `format_feed_status_block(&[FeedStatusLine])` produces:

```
Market-data feeds:
  Dhan: 776 instruments — last tick 0s ago ✓
  Groww: 768 instruments — last tick 2s ago ✓
```

Unknowns render honestly ("instruments unknown", "no tick seen yet"); ages
≤30s get a ✓, 31–120s render plain seconds, >120s render minutes (no false
red after market close). An empty list renders "Market-data feeds: unknown".
The block is appended to `WebSocketPoolOnline` (the "live and ready to trade"
readiness message) and `EndOfDayDigest`.

At the emit sites (crates/app/src/main.rs), `build_feed_status_lines` iterates
`Feed::ALL`, filters to `feed_runtime.is_enabled(feed)`, and reads each feed's
subscribed counts + last-tick age from `FeedHealthRegistry::snapshot` — the
identical single source of truth the portal `/feeds` endpoint uses. The Groww
lane already records subscribed counts + tick liveness; the Dhan lane gains a
one-shot boot-time `set_subscribed(Feed::Dhan, …)` from the subscription-plan
summary so its count is real, and a real-tick-age fallback via the existing
global tick-gap detector.

`FeedInstrumentsLoaded` is a new Severity::Info event fired from the Groww
activation `Ok(set)` arm (`groww_activation.rs`), carrying subscribed /
indices / stocks / skipped, loaded through the already-existing
`groww_sidecar_notifier_slot` (an `ArcSwapOption` filled once the
NotificationService boots — a not-yet-filled slot skips the Telegram, never
blocks activation).

## Edge Cases

- Groww enabled but not yet instrumented (cold pre-open) → line renders
  "instruments unknown — no tick seen yet"; never fabricated.
- Groww disabled → no Groww line at all (a switched-off feed is intentional,
  not "unknown").
- EOD digest fires at 15:31:30 when ticks have legitimately stopped → ages
  >120s render as minutes with no fault emoji (no false-red after close).
- Notifier slot not yet filled when the Groww watch-set resolves (boot
  ordering) → instruments-load Telegram is skipped for that activation; the
  log line still records the counts (existing `[feeds] Groww watch-list ready`).
- Dhan enabled but pool deferred off-hours → Dhan line shows subscribed count
  (set at plan build) + "no tick seen yet" honestly.
- Counter reset / clock step → `last_tick_age_secs` clamps at 0 (registry
  already guards backward clock steps).

## Failure Modes

- FeedHealthRegistry unwired for a feed → snapshot yields zeros/None → the
  line renders "unknown" (audit Rule 11: no false-OK, and equally no
  fabricated numbers).
- Groww watch-set build fails → no instruments-load event fires (the existing
  pull-until-success loop + error logs cover the failure path); the event only
  fires on the genuine `Ok(set)` arm.
- Telegram dispatch failure → existing TELEGRAM-01 drop accounting applies;
  no new failure mode is introduced.
- All changes are cold-path (boot / once-per-day schedulers); a panic in the
  builders is unreachable (pure arithmetic + formatting, unit-tested).

## Test Plan

- `cargo test -p tickvault-core` — new unit tests for the feed block renderer,
  the two extended messages, and the new `FeedInstrumentsLoaded` event
  (message, topic, severity=Info); updated pinned formatting tests.
- `cargo test -p tickvault-app` — updated `ws_pool_online_truthful_emit_guard`
  constructors; new unit tests for `build_feed_status_lines` (enabled-only
  filter + honest unknowns); existing boot-symmetry + secret-manager
  source-scan guards must stay green (EndOfDayDigest emitted exactly once).
- `cargo fmt --check` + scoped clippy on the two touched crates.

## Rollback

Revert the single commit; both events return to their previous payloads and
the Groww lane simply stops emitting the instruments-load Info ping. No
schema, no config, no state migration — messages-only change. Takes effect /
rolls back at the next deploy-aws.

## Observability

- The change IS observability: the operator-facing readiness + EOD Telegram
  messages become feed-agnostic (every enabled feed named with its subscribed
  count + last-tick age), and Groww gains the same instruments-load Info ping
  Dhan already has.
- Honest labels: unknown data reads "unknown"/"no tick seen yet" — the
  renderer never invents numbers (audit Rule 11 mirror: no false-OK, no
  false data).
- No new Prometheus metric / alarm / audit table: no new failure mode is
  introduced; the data shown is read from the existing `FeedHealthRegistry`
  (already backing the portal `/api/feeds/health` page). New `error!` sites:
  none (no ErrorCode needed).
- Telegram commandments kept: plain English, provider names only, emoji
  status, no file paths, no library names.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan + Groww both live at boot | readiness message lists both feeds with real counts + tick ages |
| 2 | Groww disabled | readiness + EOD show Dhan line only |
| 3 | Groww enabled, sidecar not yet streaming | Groww line shows count (from watch-set) + "no tick seen yet" |
| 4 | EOD at 15:31:30 with both feeds | per-feed block in the digest, ages in minutes, no fault emoji |
| 5 | Groww watch-set resolves | one Info Telegram: subscribed / indices / stocks / skipped |
| 6 | Registry unwired for a feed | "instruments unknown" — never fake numbers |
