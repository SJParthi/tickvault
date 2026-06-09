# Implementation Plan: TICK-SEQ-01 — replay-stable monotonic capture tiebreaker (zero-loss for indices/zero-volume + total intra-second order)

**Status:** APPROVED
**Date:** 2026-06-09
**Approved by:** Parthiban — 2026-06-09: operator raised the exact gap (same-second same-value index ticks collapse under content-hash dedup; "received_at should definitely be included") and demanded O(1) uniqueness/dedup/order covering all worst cases with no illusion.
**Crate(s) touched:** `tickvault-storage` (`tick_persistence.rs`, `ws_frame_spill.rs`), `tickvault-core` (WS read loop capture-stamp), `tickvault-common` (constants/ParsedTick capture field).

## Context / the gap (verified)

The `ticks` DEDUP key is `(ts[second], security_id, segment, payload_hash)`. `payload_hash`
fingerprints 12 value fields. Two ticks collapse iff all 12 are byte-identical in the same second.
For **indices (IDX_I)** — our subscription core — `volume=0`, `LTQ=0`, no trades; the price is the
ONLY varying field. So a same-second recurrence of the same index value (operator's live NIFTY
`23,146.45 → 23,146.75 → 23,146.45`) produces an identical `payload_hash` for the two `45` ticks →
QuestDB UPSERT overwrites → the return-to-45 tick is LOST. Same hole for any zero-volume / quote-only
same-price same-second tick. This does NOT corrupt minute candles (OHLC unchanged by a repeat value)
— which is why it slipped through — but it IS a tick-fidelity loss the operator forbids.

`received_at` CANNOT simply be re-added to the key: it was removed 2026-06-08 because it is stamped at
PROCESSING time and RE-STAMPED on WAL replay → replayed ticks get new `received_at` → duplicate rows →
broken idempotency.

## Design

Introduce a **strictly-monotonic, replay-stable `capture_seq` (u64)** stamped at the WS READ instant,
persisted INSIDE the WAL frame, and used as the dedup tiebreaker + sort key.

1. **Stamp at read:** in the WS read loop, before the WAL append, compute
   `capture_seq = max(prev_capture_seq + 1, wall_clock_nanos())` via an `AtomicU64` (`fetch_update`/CAS
   loop, lock-free, O(1), zero-alloc). Strictly increasing → never repeats, even within one nanosecond.
2. **Persist in WAL:** extend the WAL record format to carry `capture_seq` (8 bytes) so replay reuses
   the SAME value (replay-stable). Bump a WAL format version tag; old segments without the field replay
   via a back-compat path (treat missing capture_seq as 0-ordered, or re-derive — see Edge Cases).
3. **Thread to ParsedTick:** carry `capture_seq` from the frame to `ParsedTick` so the persist writer
   can stamp the column.
4. **New dedup key + column:** `ticks` gains a `capture_seq LONG` column;
   `DEDUP UPSERT KEYS(ts, security_id, segment, capture_seq)`. Keep `payload_hash` as a stored
   content-integrity column (NOT in the key). Schema self-heal via `ALTER TABLE ADD COLUMN IF NOT
   EXISTS` per the observability-architecture pattern.
5. **Order:** all read/cross-verify paths that need exact order use `ORDER BY ts, capture_seq`.

## Edge Cases (every nook + corner)

- Same second, different price → different read instant → different capture_seq → both kept.
- **Index, same value recurs (operator case)** → different capture_seq → both kept (THE fix).
- Same nanosecond / clock didn't tick → `max(prev+1, now)` clamp forces strictly-increasing → both kept.
- WAL replay after crash → capture_seq read FROM the WAL frame (not re-stamped) → true duplicate →
  same key → collapses → idempotent.
- Restart mid-second → wall clock advanced during restart (seconds) → post-restart capture_seq ≫
  pre-restart → no collision with same-second pre-restart ticks.
- NTP steps clock backward → monotonic `+1` clamp keeps capture_seq strictly increasing → no repeat.
- Old WAL segments (pre-format-change) on first boot after deploy → back-compat replay: missing
  capture_seq → assign monotonically at replay from the boot counter seeded above any existing value
  (these are recovery-only, post-crash; the dedup still protects via (ts, sid, seg) + payload_hash
  fallback for that one transitional replay). Document + test.
- u64 wrap → 1.8e19 captures; never reached. Asserted bounded.

## Failure Modes

- Hot path adds ONE atomic op + 8-byte WAL field; no heap alloc (DHAT-gated). No `unwrap`/`expect`.
- WAL format bump must not break replay of in-flight old segments → explicit versioned parser + test.
- DEDUP key change is a schema migration → `ALTER ADD COLUMN IF NOT EXISTS` + DEDUP re-enable; idempotent.
- payload_hash retained (no data-integrity regression on content fingerprint).

## Test Plan

- `crates/storage` unit: capture_seq monotonic under concurrency (loom or atomic-stress); `max(prev+1,now)`
  clamp correctness incl. backward-clock + same-nanosecond inputs.
- WAL roundtrip: new format writes + replays capture_seq exactly; old-format segment back-compat replay.
- **Chaos: `chaos_index_same_value_burst_preserved`** — replay a `45 → 75 → 45` (and N-deep) same-second
  index burst through capture→WAL→persist; assert all 3 rows survive AND `ORDER BY ts, capture_seq`
  reproduces exact arrival order. (The direct regression for the operator's screenshot.)
- Idempotency: replay the same WAL twice → no duplicate rows (capture_seq stable).
- DHAT zero-alloc on the read-loop stamp path; Criterion budget for the atomic stamp.
- DEDUP meta-guard updated: `DEDUP_KEY_TICKS` includes `capture_seq` + still `security_id`+`segment` (I-P1-11).
- data-integrity.md updated (Tick Deduplication section) — capture_seq replaces payload_hash as the key
  tiebreaker; payload_hash demoted to content column.

## Rollback

WAL format is versioned (old segments still replay). Schema change is additive (`ADD COLUMN IF NOT
EXISTS`). To roll back: revert the key to `payload_hash` (the column remains populated) — no data loss,
since payload_hash is still written. `git revert <sha>`.

## Observability

- Counter `tv_tick_capture_seq_clamps_total` (how often the `+1` clamp fired vs wall-clock advanced) —
  a sustained high rate flags a slow/backward clock (BOOT-03 territory).
- The chaos test is the ratchet; cross-verify 1m unaffected (candles already correct).

## Adversarial review (mandatory — architecturally significant)

3 agents on the design AND the diff: hot-path-reviewer (atomic stamp zero-alloc, no contention on the
read loop), security-reviewer (WAL format change, no injection), general-purpose hostile (replay
back-compat, restart-mid-second collision, NTP backward, u64 wrap, migration idempotency).

## Plan Items

- [ ] `capture_seq` monotonic stamper (AtomicU64, `max(prev+1, now)`) at WS read
  - Files: crates/core/src/websocket/connection.rs (+ common constant)
  - Tests: test_capture_seq_strictly_monotonic, test_capture_seq_clamps_backward_clock
- [ ] WAL frame format v2 carries capture_seq (+ versioned back-compat replay)
  - Files: crates/storage/src/ws_frame_spill.rs
  - Tests: test_wal_v2_roundtrip_capture_seq, test_wal_v1_backcompat_replay
- [ ] Thread capture_seq frame→ParsedTick→persist; ticks gains capture_seq column + new DEDUP key
  - Files: crates/common/src/tick_types.rs, crates/storage/src/tick_persistence.rs
  - Tests: test_dedup_key_includes_capture_seq, test_tick_dedup_key_includes_segment (unchanged)
- [ ] Chaos regression for the operator's index burst
  - Files: crates/storage/tests/chaos_index_same_value_burst_preserved.rs
  - Tests: chaos_index_same_value_burst_preserved, chaos_capture_seq_replay_idempotent
- [ ] data-integrity.md + dedup meta-guard updated
  - Files: .claude/rules/project/data-integrity.md, crates/storage/tests/dedup_segment_meta_guard.rs

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Index `45→75→45` same second | all 3 rows kept, ordered by capture_seq |
| 2 | Same nanosecond / backward clock | monotonic clamp → distinct capture_seq → both kept |
| 3 | WAL replay of same frames twice | idempotent — no duplicate rows |
| 4 | Restart mid-second | post-restart capture_seq ≫ pre-restart → no collision |
| 5 | Old-format WAL segment on first boot | back-compat replay, no panic, no loss |
