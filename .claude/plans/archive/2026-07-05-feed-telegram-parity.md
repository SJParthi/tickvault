# Implementation Plan: Feed Telegram Parity — deferred boot pings + per-feed badge

**Status:** APPROVED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator directive 2026-07-05, verbatim: "why dhan messages and groww messages are not same... dhan and groww should be uniquely seen to view easily")

> Guarantee matrices: this plan cross-references the canonical 15-row 100% guarantee
> matrix + 7-row resilience demand matrix in
> `.claude/rules/project/per-wave-guarantee-matrix.md` (see Item 22 / Item 24 /
> per-wave-guarantee-matrix.md). Any 100% claim here is 100% inside the tested
> envelope, with ratcheted regression coverage — the queue is capacity-bounded
> (`MAX_PENDING_BOOT_PINGS`), the flusher budget-bounded, and the badge set pinned by
> build-failing tests; no delivery claim is made for a notifier that never constructs
> (strict-init refuses boot in that case, by design).

## Design

**Part 1 — root cause (Verified, operator's Mac Build 83dc65d9 app log):** the Groww
boot-stage Telegram pings (`FeedAuthOk`, `FeedInstrumentsLoaded`) are emitted through the
lazily-filled `groww_sidecar_notifier_slot` (`Arc<ArcSwapOption<NotificationService>>`,
declared `crates/app/src/main.rs:574`). The slot is filled ONLY on the FAST boot path
(`main.rs:1705`). The slow-boot / GROWW-ONLY path builds its notifier inside
`build_shared_infra` (`main.rs:5317`) and `main()` destructures it at `main.rs:2616` —
but NEVER stores it into the slot. So `groww_activation::notify_when_ready` polls for its
2-minute budget, gives up, and logs exactly the operator's evidence line
(`boot-stage Telegram skipped — notifier slot never filled within the wait budget`).

Fix, two layers:
1. **Fill the slot in the hoisted shared-infra prefix** (provably safe: the notifier is
   fully constructed — strict init + coalescer wrap — before the store; identical
   pattern to the existing fast-path store at `main.rs:1705`). One line after the
   `build_shared_infra` destructure. This also un-blinds the sidecar-reject and
   bridge-connected pings on slow boots (they lazily load the same slot).
2. **Queue-and-flush instead of give-up** in `crates/app/src/groww_activation.rs`: a new
   `PendingBootPings` bounded FIFO (`MAX_PENDING_BOOT_PINGS = 16`, `std::sync::Mutex` +
   `VecDeque`) replaces the per-event give-up poll. `notify_or_queue(slot, event)` sends
   immediately when the notifier is present (flushing any stale queue first, order
   preserved); otherwise queues (drop-oldest at capacity, warn). A single bounded
   `flush_when_ready` task (same 500ms x 240 budget constants, NO busy-wait — sleeps
   between polls) flushes the queue the moment the slot fills and emits an `info!` per
   late-flushed ping; on budget expiry it clears the queue loudly (bounded memory).
   A post-queue slot re-check closes the store-vs-queue race without memory-ordering
   subtleties.

**Part 2 — per-feed visual identity:** new `crates/core/src/notification/feed_badge.rs`
(mirror of `source_badge.rs`): `FeedBadge::{Dhan, Groww}` → `"🔷 DHAN"` / `"🟢 GROWW"`,
plus case-insensitive `feed_badge_for_name`. `NotificationEvent` gains
`feed_badge() -> Option<&'static str>`: static Dhan badge for Dhan-scoped events (auth /
token / profile, main-feed + order-update WebSocket lifecycle, instrument build), static
Groww badge for `GrowwSidecarRejected`, dynamic (from the `feed` field) for
`FeedAuthOk` / `FeedInstrumentsLoaded` / `FeedConnectedAwaitingTicks`. `to_message()` is
split into a private `message_body()` (the existing match, unchanged) + a wrapper that
prepends `"{badge} — "` when `feed_badge()` is `Some`. Because BOTH dispatch paths
(immediate bypass + coalescer samples) derive from `to_message()`, the badge can never
drift between paths, and mixed-feed coalescer buckets stay correct (badge rides inside
each sample). Non-feed events are byte-identical (badge = `None`). The 10 Telegram
commandments hold: severity emoji stays first (prefix layer untouched), badge is one
emoji + one plain word, no paths/jargon.

## Edge Cases

- Groww activation reaches `FeedAuthOk` BEFORE `build_shared_infra` finishes (Docker
  ensure can take tens of seconds): event queues, flushes late with an `info!`.
- Notifier stored between the empty-slot check and the queue push: post-queue re-check
  flushes immediately — no stranded event.
- More than 16 boot-stage events queue before the notifier exists: drop-OLDEST with a
  `warn!` naming the dropped topic — bounded memory, newest milestones win.
- Budget expiry with events still queued (notifier never arrives — regression class):
  queue cleared with a `warn!` carrying the count; later `notify_or_queue` calls with a
  live slot still send directly (self-healing, no task required).
- Runtime re-enable hours after boot: slot long filled → `notify_or_queue` sends
  directly; queue and flusher are inert.
- Unknown future feed name in `FeedAuthOk{feed}`: `feed_badge_for_name` returns `None` —
  message renders exactly as today (honest, no wrong badge).
- Coalesced bucket mixing feeds on one topic: each sample carries its own badge (badge
  applied at `to_message()` time, not at bucket level).
- Mutex poisoning on the pending queue: recovered via `unwrap_or_else(|p| p.into_inner())`
  (cold path, events are plain data — no invariant to corrupt).

## Failure Modes

- Notifier slot never filled on some future boot path: pings queue, flusher budget
  expires → loud `warn!` with count + topics remain in the structured log (the
  milestone `info!` lines still fire independently). No unbounded memory, no hang.
- `NotificationService` in disabled/no-op mode: `notify()` is a no-op by design —
  queue-and-flush delivers to it and returns; no retry storm.
- Flush partially interleaves with a new direct send: `notify_or_queue` drains the queue
  BEFORE sending the current event, so boot-stage order is preserved per producer.
- Badge regression on non-feed events: ratcheted by
  `test_non_feed_events_carry_no_feed_badge` (StartupComplete / QuestDb / IP events).

## Test Plan

Pure-logic tests (no network, no Telegram):
- `test_notify_or_queue_queues_before_notifier_then_flushes_in_order` — 3 events queued
  pre-notifier; after the slot fills, flush returns their topics in FIFO order.
- `test_notify_or_queue_sends_directly_when_notifier_present` — filled slot → `Sent`.
- `test_notify_or_queue_drops_oldest_at_capacity` — cap+2 events → queue length pinned
  at `MAX_PENDING_BOOT_PINGS`, oldest dropped, newest retained.
- `test_flush_when_ready_flushes_after_slot_fills` — async, tiny budget (mirrors the old
  `notify_when_ready` delivery test).
- `test_flush_when_ready_budget_expiry_clears_queue_bounded` — never-filled slot →
  returns false, queue emptied (bounded memory).
- `test_notify_or_queue_recheck_flushes_after_racing_store` — store lands right after
  queuing; the re-check path flushes.
- Badge: `test_feed_badge_dhan_and_groww_are_distinct_short_plain`,
  `test_feed_badge_for_name_case_insensitive_and_unknown_none`,
  `test_dhan_scoped_events_carry_dhan_badge_in_message`,
  `test_groww_feed_events_carry_groww_badge_in_message`,
  `test_dynamic_feed_events_badge_follows_feed_field`,
  `test_non_feed_events_carry_no_feed_badge`.
- Scoped runs: `cargo test -p tickvault-core --lib notification`,
  `cargo test -p tickvault-app --lib`, plus `cargo fmt --check` + scoped clippy.

## Rollback

Both parts are additive and independently revertible: revert the single slot-store line
in `main.rs` to restore the old (broken) slow-path behaviour; revert the
`groww_activation.rs` queue module to restore the bounded give-up poll; revert
`feed_badge.rs` + the `to_message()` wrapper to restore badge-less messages. No schema,
no config, no persisted state — a plain `git revert` of the PR is a complete rollback.

## Observability

- `info!` per late-flushed ping (`deferred boot-stage Telegram flushed late`), carrying
  the topic — the operator-visible proof the race was absorbed.
- `warn!` on drop-oldest (dropped topic named) and on budget-expiry clear (count named).
- The existing skip `info!` line is retired with the give-up path it described.
- The badges themselves are the operator-facing observability: every feed-scoped
  Telegram now answers "which feed?" at a glance (`🔷 DHAN` / `🟢 GROWW`), uniform across
  the immediate and coalesced dispatch paths.
- No new metrics: cold-path boot pings; the notification layer's existing
  `tv_telegram_dispatched_total` / `tv_telegram_dropped_total` counters are unchanged.

## Plan Items

- [x] Item 1 — Fill the notifier slot in the hoisted shared-infra prefix (slow/GROWW-ONLY boot path)
  - Files: crates/app/src/main.rs
  - Tests: covered by existing boot wiring ratchets; line mirrors main.rs:1705 fast-path store

- [x] Item 2 — Queue-and-flush deferred boot pings (replaces give-up `notify_when_ready`)
  - Files: crates/app/src/groww_activation.rs
  - Tests: test_notify_or_queue_queues_before_notifier_then_flushes_in_order, test_notify_or_queue_sends_directly_when_notifier_present, test_notify_or_queue_drops_oldest_at_capacity, test_flush_when_ready_flushes_after_slot_fills, test_flush_when_ready_budget_expiry_clears_queue_bounded, test_notify_or_queue_recheck_flushes_after_racing_store

- [x] Item 3 — Per-feed visual badge on every feed-scoped Telegram
  - Files: crates/core/src/notification/feed_badge.rs, crates/core/src/notification/mod.rs, crates/core/src/notification/events.rs
  - Tests: test_feed_badge_dhan_and_groww_are_distinct_short_plain, test_feed_badge_for_name_case_insensitive_and_unknown_none, test_dhan_scoped_events_carry_dhan_badge_in_message, test_groww_feed_events_carry_groww_badge_in_message, test_dynamic_feed_events_badge_follows_feed_field, test_non_feed_events_carry_no_feed_badge

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Slow boot, Groww auth OK at T+5s, notifier at T+40s | FeedAuthOk queued, flushed at T+40s with info! — Telegram arrives |
| 2 | Fast boot (notifier early) | notify_or_queue sends directly; behaviour identical to today minus the race |
| 3 | Notifier never arrives (regression) | budget-expiry warn! + cleared queue; bounded memory; structured logs intact |
| 4 | Dhan auth ping | "✅ [LOW] 💻 LOCAL 🔷 DHAN — Auth OK — Dhan JWT acquired" style: feed visible at a glance |
| 5 | Groww instruments ping | "🟢 GROWW — ✅ Groww instruments loaded ..." |
| 6 | Non-feed event (QuestDB, boot, risk) | Byte-identical message — no badge |
