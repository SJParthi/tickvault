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

Five defenses, one per silent signal, all gated to the trading session and
edge-triggered natively by CloudWatch (Rule 4):

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md`
(the 15-row + 7-row matrices apply as cross-referenced per that file's
"or cross-reference it" clause). This is a single-file, cold-path routing
fix — no hot path (no DHAT/bench delta), no new table, no new pub fn, no
new failure mode, no security surface; the inline ratchet test
`test_market_open_confirmations_are_immediate_info` is the extreme-check
row's build-failing artefact.
