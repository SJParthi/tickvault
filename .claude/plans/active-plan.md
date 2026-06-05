# Implementation Plan: Time-windowed ring dedup (hardening — NOT the 23440 candle fix)

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban ("Do the dedup hardening anyway")
**Crate(s) touched:** `core`

## Context (verified, no illusion)

The in-memory `TickDedupRing` (`tick_processor.rs`) drops a tick when its exact
`(security_id, exchange_timestamp, ltp)` fingerprint was already seen — to drop
Dhan reconnect re-sends. Side effect: a **genuine price re-touch** with a
**stale LTT** (same `exchange_timestamp`) is wrongly dropped, so it never reaches
the ticks table or the broadcast.

**Honest scope (stated up-front, in the PR too):** this does **NOT** fix the
11:49 23440 candle-high miss. Verified: the candle aggregator buckets by
`exchange_timestamp` (L-H7) and DISCARDS ticks whose LTT precedes the open bucket
(L-C3). So a stale-LTT re-touch, even if kept here, is bucket-discarded by the
aggregator. This hardening only restores the re-touch to the **`ticks` table**
(visible by `received_at`); the candle fix (if the cause is stale LTT) is a
separate bucketing change pending live-data confirmation.

## Plan Items

- [x] Item 1 — Make the ring time-windowed (slot stores last-seen nanos)
  - Files: `crates/core/src/pipeline/tick_processor.rs`, `crates/common/src/constants.rs`
  - Tests: `TickDedupRing` unit tests (window in/out, distinct ltp)
- [x] Item 2 — Pass `received_at_nanos` at the two call sites
  - Files: `crates/core/src/pipeline/tick_processor.rs`

## Design

1. `TickDedupRing.slots: Box<[(u64, i64)]>` — (fingerprint, last_seen_nanos),
   init `(u64::MAX, i64::MIN)`.
2. `is_duplicate(security_id, exchange_timestamp, ltp, now_nanos) -> bool`:
   `recent = fp == key && now_nanos.saturating_sub(last) <= DEDUP_RESEND_WINDOW_NANOS`;
   always store `(key, now_nanos)`; return `recent`. A re-send (within window) →
   dropped; a genuine re-touch beyond the window → kept.
3. `DEDUP_RESEND_WINDOW_NANOS = 2_000_000_000` (2 s) in `constants.rs` —
   comfortably covers a reconnect re-send burst (sub-second) while letting a
   real re-touch seconds/minutes later through. Dropping an identical
   `(price, LTT)` within 2 s is OHLC-safe (the price is already recorded).
4. Both call sites pass `tick.received_at_nanos` as `now_nanos` (the arrival
   clock — the dedup window is in arrival terms, which is what distinguishes a
   reconnect re-send from a later genuine re-touch).
5. `fingerprint()` unchanged.

## Edge Cases

- **Reconnect gap > window** → one re-send leaks through (kept). Acceptable: it
  re-broadcasts an identical price (no OHLC change) + one extra ticks row
  (distinct `received_at`). Rare; documented.
- **received_at non-monotonic (NTP step back)** → `saturating_sub` floors at 0 →
  treated as "recent" → dropped. Safe (worst case drops a re-touch, never panics).
- **Slot collision** (different fingerprint, same idx) → still can only cause a
  MISSED dup (a tick passes), never a false drop. Unchanged from before.

## Failure Modes

- Window too long → a fast genuine re-touch of identical (price,LTT) dropped —
  but that's OHLC-safe (same price already seen). Window too short → re-sends
  leak as extra rows. 2 s balances both.
- The ring is still O(1), zero-alloc (pre-allocated `Box<[_]>`), `#[inline]` —
  hot-path budget unchanged. Enforced by the DHAT + Criterion gates.

## Test Plan

- `cargo test -p tickvault-core tick_processor` — new/updated `TickDedupRing`
  tests: (a) identical within window → dup; (b) identical beyond window → NOT
  dup; (c) different ltp → not dup; (d) first-seen → not dup.
- DHAT zero-alloc + Criterion bench (the mechanical hot-path Z+ gates) — the
  change adds one i64 compare + store, no allocation; budget must hold (≤5%).
- `cargo clippy --workspace -- -D warnings -W clippy::perf` (real CI command).
- design-first wall (impl crate `core` + this APPROVED plan).

## Rollback

Single-file logic + one constant. Revert restores the timestamp-less ring. No
schema/data change. No candle-behaviour change (the candle never benefited).

## Observability

The existing `tv_dedup_filtered_total` counter now reflects only true re-sends
(within window), not genuine re-touches — so its rate becomes a cleaner re-send
signal. No new metric.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item are bound by the 15-row 100% guarantee matrix and the
7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" means "100%
inside the tested envelope, with ratcheted regression coverage" — the unit tests
+ DHAT + Criterion gate. This is a hot-path change kept O(1)/zero-alloc; it
hardens ticks-table re-touch fidelity and **does NOT fix the 23440 candle-high
miss** (the aggregator bucket-discards stale-LTT ticks). Stating it fixes the
candle would be the hallucination the operator forbids.
