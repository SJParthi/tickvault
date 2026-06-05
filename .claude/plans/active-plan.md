# Implementation Plan: Conform in-memory dedup ring to the DECIDED tick identity

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban ("we have already decided everywhere especially for ticks which is segment security id and timestamp and received at" / "dhan will never resend anything ... tick by tick event stream ... this one we have designed already")
**Crate(s) touched:** `core`, `common`

## Context (decided design, no illusion)

Dhan is a **tick-by-tick EVENT STREAM** and **never re-sends** anything
(operator-confirmed, Dhan docs "tick-by-tick event based data"). The tick
identity is **`(exchange_segment_code, security_id, exchange_timestamp,
received_at_nanos)`** and is used the SAME way everywhere: in-memory RAM, the
in-memory ring, the disk spill, the WAL frame log, and the `ticks` table DEDUP
key (`(security_id, segment, received_at)`). `received_at_nanos` is the local
arrival clock — unique per arriving frame.

The previous `TickDedupRing` keyed on `(security_id, exchange_timestamp, ltp)`
(+ a 2 s time window from #1025). That premise — "drop reconnect re-sends" — is
WRONG: there are no re-sends to drop. Worse, an `ltp`-based key could false-drop
a genuine price re-touch (the operator's 11:49 23440 case).

**This PR conforms the ring to the decided identity.** Because `received_at` is
unique per arrival, two genuine ticks can NEVER share a fingerprint → the ring
can NEVER false-drop a genuine tick ("keep every tick"). It now fires `true`
ONLY for a true byte-identical double-delivery of the same frame (which the
stream does not produce). It is an O(1) belt-and-suspenders identity guard, not
a re-send filter. `exchange_segment_code` is the FIRST identity field per
I-P1-11 (security_id is not unique across segments).

**Honest scope:** this guarantees zero in-RAM false-drops and aligns the RAM
ring with the storage DEDUP key + WAL/spill identity. It does NOT, by itself,
change candle bucketing (the aggregator buckets by `exchange_timestamp` and
discards stale-LTT ticks — that is a separate, live-data-pending item).

## Plan Items

- [x] Item 1 — Re-key the ring to `(segment, security_id, ts, received_at)`
  - Files: `crates/core/src/pipeline/tick_processor.rs`
  - Tests: `test_dedup_ring_*` (new-tick, exact-same-frame, different-received_at-kept,
    different-segment, different-security, different-timestamp, zero, max,
    negative-received_at, eviction, interleaved, large-buffer, fingerprint set)
- [x] Item 2 — Slots `Box<[u64]>` (drop the `(u64, i64)` window pair); init `u64::MAX`
  - Files: `crates/core/src/pipeline/tick_processor.rs`
- [x] Item 3 — Update both production call sites to pass the 4-tuple
  - Files: `crates/core/src/pipeline/tick_processor.rs`
- [x] Item 4 — Remove the now-dead `DEDUP_RESEND_WINDOW_NANOS` constant
  - Files: `crates/common/src/constants.rs`

## Design

1. `TickDedupRing.slots: Box<[u64]>`, init `u64::MAX` (empty sentinel).
2. `fingerprint(exchange_segment_code: u8, security_id: u32, exchange_timestamp: u32, received_at_nanos: i64) -> u64`
   — FNV-1a, segment mixed FIRST (I-P1-11), `received_at as u64` last field.
3. `is_duplicate(seg, id, ts, received_at) -> bool`: `seen = slots[idx] == key`;
   always store `key`; return `seen`. No time math, no `now_nanos` arg.
4. Both call sites pass `tick.exchange_segment_code, tick.security_id,
   tick.exchange_timestamp, tick.received_at_nanos`.
5. Hot path stays O(1), zero-alloc: one hash + one compare + one store. The
   slot is now a single `u64` (8 bytes) instead of `(u64, i64)` (16) — half the
   per-slot footprint and one fewer field to write.

## Edge Cases

- **Genuine price re-touch (stale LTT, fresh arrival)** → fresh `received_at` →
  distinct fingerprint → KEPT. (The 23440 class — no longer droppable in RAM.)
- **Same security_id on a different segment** (Dhan reuses ids, e.g. 27) →
  distinct fingerprint → KEPT. (I-P1-11.)
- **True byte-identical double-delivery** (same 4-tuple incl. received_at) →
  `true` → dropped. The stream does not produce this; guard is defensive.
- **Negative / `i64::MIN` received_at** (clock step-back) → `as u64` is
  well-defined, no panic; just a distinct fingerprint.
- **Slot collision** (different fingerprint, same idx) → can only cause a MISSED
  dup (a frame passes), never a false drop. Storage DEDUP is the authority.

## Failure Modes

- Ring can NEVER false-drop a genuine tick (received_at uniqueness is the proof).
- If a true duplicate is ever evicted by collision, it passes to storage, where
  the `ticks` DEDUP UPSERT KEYS `(security_id, segment, received_at)` dedupe it
  — same identity, so RAM and storage agree.
- O(1), zero-alloc, `#[inline(always)]` — hot-path budget held (one fewer
  store than before). DHAT + Criterion gates enforce.

## Test Plan

- `cargo test -p tickvault-core dedup_ring` — 17 ring tests green (incl. the
  decided-key cases: different-received_at KEPT, different-segment KEPT,
  exact-same-frame dup).
- `cargo test -p tickvault-core --lib` (2029) + `cargo test -p tickvault-common --lib` (874) green.
- `cargo clippy -p tickvault-core -p tickvault-common -- -D warnings -W clippy::perf` clean.
- `bash .claude/hooks/banned-pattern-scanner.sh` exit 0.
- design-first wall (impl crates `core` + `common`, this APPROVED plan).

## Rollback

Single ring rewrite + one removed constant. Revert restores the prior ring. No
schema/data change; the `ticks` DEDUP key is unchanged (storage already keys on
`(security_id, segment, received_at)` — this PR only aligns RAM to it).

## Observability

`tv_dedup_filtered_total` now counts ONLY true byte-identical double-deliveries
(expected ≈ 0 in production, since Dhan never re-sends). A non-zero rate becomes
a clean anomaly signal (genuine double-delivery / upstream bug), not noise from
re-touches. No new metric.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item are bound by the 15-row 100% guarantee matrix and the
7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" means "100%
inside the tested envelope, with ratcheted regression coverage" — the 17 ring
unit tests + DHAT + Criterion gate. This hot-path change is O(1)/zero-alloc and
guarantees **zero in-RAM false-drops** by keying on the unique-per-arrival
`received_at` (the decided identity, matching storage + WAL + spill). It does
**NOT** change candle bucketing — claiming it fixes the 23440 candle-high miss
would be the hallucination the operator forbids.
