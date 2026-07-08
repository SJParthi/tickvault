# Implementation Plan: Market-open confirmations bypass the in-market digest

**Status:** VERIFIED
**Date:** 2026-07-08
**Approved by:** Parthiban (operator directive 2026-07-08 — verbatim complaint
"why every telegram notification is very late" + standing merge instruction)
**Changed crates:** core (crates/core — single file
`crates/core/src/notification/events.rs` + its inline tests)

## Design

PR #1439 introduced the in-market digest lane (`DispatchLane::Digest`,
900s window — `crates/core/src/notification/coalescer.rs:74`
`DEFAULT_DIGEST_WINDOW_SECS`, `config/base.toml` `digest_window_secs = 900`)
that batches Low/Medium/Info chatter during market hours. Verified incident
2026-07-08 morning: the digest also swept the three once-per-trading-day
market-open confirmations, so the operator received "READY for market open
@ 09:14" at 09:28, "Streaming live @ 09:15:30" at 09:30, and "self-test
PASSED @ 09:16" at 09:30. These are time-critical at the open and must be
instant.

Fix: in `crates/core/src/notification/events.rs`,
`NotificationEvent::dispatch_policy()` already carries a
`DispatchPolicy::Immediate` arm for exactly this once-per-day
must-arrive-now class (AuthenticationSuccess, WebSocketPoolOnline,
OrderUpdateConnected, CrossVerify1mSummary, ...). Add the three variants:

- `Self::MarketOpenReadinessConfirmation { .. }`
- `Self::MarketOpenStreamingConfirmation { .. }`
- `Self::SelfTestPassed { .. }`

`DispatchPolicy::Immediate` wins over severity in `classify_dispatch`
(`crates/core/src/notification/coalescer.rs:146-148`), so no severity
change is needed — the three stay `Severity::Info` (color decoupled from
routing, the established 2026-05-09 pattern).

## Edge Cases

- `SelfTestDegraded` / `SelfTestCritical` are deliberately NOT added —
  they are Severity::High / Critical already, which routes immediate via
  the severity fallback in `classify_dispatch`.
- `MarketOpenStreamingFailed` is Severity::High → already immediate;
  untouched.
- The three variants stay Severity::Info so their existing tag/color and
  Telegram wording are unchanged — only routing changes.
- Off-market emission: these events fire only at 09:14 / 09:15:30 / 09:16
  IST on trading days (their emitters are edge-triggered once per day),
  so the Immediate policy adds no new spam surface.

## Failure Modes

- Regression risk: a future edit removes a variant from the Immediate arm
  → the new inline ratchet test asserting
  `dispatch_policy() == DispatchPolicy::Immediate` for each of the three
  variants fails the build.
- Wrong-shape risk: matching a struct variant without `{ .. }` does not
  compile — the compiler enforces the exact variant shape.
- No runtime failure path: `dispatch_policy()` is a pure total match over
  `&self`; adding arms cannot panic, allocate, or change any other arm.

## Test Plan

- Extend the existing dispatch-policy test coverage in the inline
  `#[cfg(test)]` module of `crates/core/src/notification/events.rs`:
  `test_market_open_confirmations_are_immediate_info` constructs minimal
  instances of all three variants and asserts
  `dispatch_policy() == DispatchPolicy::Immediate` AND
  `severity() == Severity::Info` (severity unchanged), following the
  existing `test_per_ws_connect_events_are_immediate_low` style.
- Gates: `cargo fmt --check`;
  `cargo clippy -p tickvault-core --no-deps -- -D warnings`;
  `cargo test -p tickvault-core --lib notification`.

## Rollback

Single-commit change to one file — `git revert` of the commit restores the
pre-fix routing (the three confirmations coalesce into the 900s digest
again). No schema, config, wire-format, or Telegram-wording change; no
data migration; no coalescer change.

## Observability

- The change itself IS an observability fix: the three once-per-day
  operator pings in crates/core now arrive at their emission instants
  (09:14 / 09:15:30 / 09:16 IST) instead of up to ~15 minutes late inside
  the in-market digest.
- No new counters/logs needed: the coalescer's existing dispatch metrics
  and the events' existing rendering are untouched; the new inline
  ratchet test pins the routing so a silent regression fails the build.

## Plan Items

- [x] Add the 3 variants to the `DispatchPolicy::Immediate` arm of
      `dispatch_policy()`
  - Files: crates/core/src/notification/events.rs
  - Tests: test_market_open_confirmations_are_immediate_info
- [x] Inline ratchet test covering all 3 variants (Immediate + Info)
  - Files: crates/core/src/notification/events.rs
  - Tests: test_market_open_confirmations_are_immediate_info

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | MarketOpenReadinessConfirmation emitted at 09:14 IST in-market | Delivered instantly (Immediate), stays [INFO] |
| 2 | MarketOpenStreamingConfirmation emitted at 09:15:30 IST | Delivered instantly, stays [INFO] |
| 3 | SelfTestPassed emitted at 09:16 IST | Delivered instantly, stays [INFO] |
| 4 | SelfTestDegraded / SelfTestCritical | Unchanged — High/Critical severity already routes immediate |
| 5 | Low/Medium in-market chatter | Unchanged — still digest-batched (900s window) |

## Per-Item Guarantee Matrix

## Honest 100% Claim (operator-charter §F wording)

> 100% inside the tested envelope, with ratcheted regression coverage:
> the four 2026-07-06 silent signals each gain a window-gated, notBreaching,
> M-of-N-latched CloudWatch page that fires within 10–15 minutes on an
> incident replay (thresholds pinned by build-failing ratchets in
> `crates/common/tests/cloudwatch_app_alarms_wiring.rs`); the lag ring write
> is DHAT-zero-alloc + Criterion-budgeted O(1) on the hot path while the
> p99 computation is honestly O(N-window), N≤32,768, on a cold 10s task;
> the ≤100,000-tick rescue ring / spill / DLQ chain is untouched (no new
> tick-drop path). Beyond the envelope: the boundary-catchup threshold 2000
> is PROVISIONAL until a one-trading-week soak measures the healthy Dhan
> floor; sub-second Dhan wire lag is UNMEASURABLE (u32 whole-second LTT
> floor); the alarms depend on the window-gate Lambda being alive (its
> failure leaves them dark — accepted and documented); and lag above 6h is
> invisible to the portal panel by the documented partition-pruning ceiling.

## Open Questions (with shipped defaults)

1. **QuestDB `now()` timezone** — Assumed UTC (matches storage code:
   received_at is written `Utc::now()+IST_UTC_OFFSET_NANOS`, i.e. the DB
   clock is UTC and rows are IST-shifted). Ship WITH
   `dateadd('m',330,now())`; best-effort live `select now()` probe; if the
   box returns IST, drop the +330m terms — nothing else changes.
2. **capture_seq wall-clock assumption** — Assumed: capture_seq ≈ UTC wall
   nanos at the original WS-read instant (`max(prev+1, wall_nanos)` per
   data-integrity.md) and is PRESERVED through WAL re-injection. Spot-check
   on live data pre-merge (compare capture_seq vs received_at on fresh
   rows).
3. **Boundary-catchup healthy Dhan floor** — Unknown. Ship PROVISIONAL 2000
   (2.25–2.9× below incident rate) + mandatory 1-trading-week soak of the
   exported per-feed Sum(5m) distribution; ratchet with a dated note if the
   healthy floor approaches 2000.
4. **M-of-N deviation from literal "sustained N×M" wording** — Deliberate:
   strict-consecutive evaluation on oscillating data (the incident logged
   125 SLO-band crossings, band 29-67 silent/min; a threshold-adjacent
   value flapping 39/41/39) would never latch and reproduces the miss
   (Rule-11 false-OK). 10-of-12 / 9-of-15 are the honest latches (9-of-15
   is the round-2 correction: freshness-only breach needs ≥39 of 776
   silent, so incident minutes at 29–38 silent sample Healthy and a
   12-of-15 latch could miss the marginal band); Item 4
   keeps strict 10/10 because its metric is self-smoothing.
5. **Medium-tier Telegram rendering** — Limitation: telegram-webhook
   handler.py stamps the same emoji on any ALARM; a Medium-vs-Critical
   visual split is a FLAGGED FOLLOW-UP, not in this PR.
6. **Alarm-count ratchet scan scope** — Verified file-scoped to
   `app-alarms.tf` (`alarm_metric_names()` reads only that file); MUST be
   extended in this PR to also scan `silent-feed-alarms.tf`, else the new
   alarms escape the three-way drift guard.
7. **Gate-Lambda single point of coupling** — Accepted risk: all four
   alarms are dark if the window-gate Lambda dies; it already gates the
   existing liveness alarms and is itself covered by the
   market-hours-liveness machinery.
8. **Correlated-page fatigue** — Accepted by design: one real feed
   degradation may fire up to 4 pages; after a zero-page all-day incident,
   redundancy is the chosen failure direction.
9. **Seconds-vs-milliseconds units** — Seconds chosen for the lag gauge:
   Dhan LTT has a ≥1s quantization floor, so millisecond units would imply
   false precision; the name carries `_seconds` and the module doc states
   the floor.
