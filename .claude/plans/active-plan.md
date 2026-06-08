# Implementation Plan: ticks DEDUP tiebreaker → content `payload_hash` (replay-safe uniqueness)

**Status:** APPROVED
**Date:** 2026-06-08
**Approved by:** Parthiban — AskUserQuestion 2026-06-08 "Frame-payload hash (Recommended)" + "fix everything".
**Crate(s) touched:** `tickvault-storage` (+ data-integrity rule).

## Context

Deep audit of the tick-ingest path found the one genuine internal loss/duplicate suspect: the
`ticks` DEDUP key `(ts, security_id, segment, received_at)` used `received_at` (local `Utc::now()`
nanos, stamped at processing time) as the sub-second tiebreaker. Two flaws:
1. **Not unique by construction** — relied on the wall clock returning a distinct nanosecond per
   tick; two distinct same-second ticks could in theory collide → one silently lost.
2. **Replay-unsafe** — on WAL/reconnect replay the same frame is re-stamped with a NEW `received_at`
   → the key differs → a true duplicate would NOT collapse → duplicate row.
Dhan's protocol has NO per-tick sequence number, so a content fingerprint is the only deterministic,
replay-stable uniqueness basis.

## Design

1. **`tick_payload_hash(&ParsedTick) -> i64`** — deterministic FNV-1a (FIXED offset basis, NOT
   `DefaultHasher`) over the value-bearing fields (ltp, exchange_timestamp, ltq, atp, volume,
   buy/sell qty, oi, day o/h/l/c). Zero-alloc, `#[inline]`, O(1) over a fixed field set.
2. **`DEDUP_KEY_TICKS`** `(ts, security_id, segment, received_at)` → `(ts, security_id, segment,
   payload_hash)`. `received_at` stays a stored COLUMN (latency analysis), removed from the key.
3. **DDL** adds `payload_hash LONG`; **brownfield migration** `ALTER TABLE ticks ADD COLUMN IF NOT
   EXISTS payload_hash LONG` runs BEFORE the DEDUP ENABLE (so the existing-table auto-recover
   DROP+CREATE path can never fire on the new key — no data loss).
4. **Doc + data-integrity rule** updated to the new key + the determinism/replay rationale.

Result: distinct ticks differ in ≥1 field → different hash → BOTH kept (no loss); a true duplicate /
WAL-replay / reconnect re-send is byte-identical → same hash → collapsed (idempotent). Achieves the
"a distinct tick can never be collapsed" goal AND fixes replay idempotency.

## Edge Cases

- Two same-second different-price ticks → different hash → both kept. ✅
- Same frame replayed with a fresh `received_at` → same hash → collapsed. ✅ (test pins this)
- Same-second identical-content ticks → collapse (information-identical duplicate — correct).
- `usize`/overflow: FNV uses `wrapping_mul`; `u64`→`i64` via bit-reinterpret (full range, no panic).
- Brownfield table missing the column → ADD COLUMN IF NOT EXISTS first → DEDUP never hits
  "key column not found" → no accidental DROP TABLE.

## Failure Modes

- ADD COLUMN fails (QuestDB down at boot) → logged WARN; DEDUP enable may then auto-recover. The
  app's own ring→spill→DLQ still protects ticks. No new panic path (no unwrap/expect added).
- Hot path: + ~48 byte-ops/tick, zero-alloc — negligible; DHAT-safe.

## Test Plan

`cargo test -p tickvault-storage --lib tick_persistence::tests` (271 pass) + new hash tests:
- `test_payload_hash_is_deterministic_across_fresh_ticks`
- `test_payload_hash_ignores_received_at_replay_safe` (the replay-safety guarantee)
- `test_payload_hash_differs_on_distinct_price` / `_volume` / `_exchange_timestamp`
- `test_tick_dedup_key_includes_segment` updated (key == `security_id, segment, payload_hash`)
- `dedup_segment_meta_guard` (5) still green (key includes segment).

## Rollback

Revert the commit. The `payload_hash` column is additive (harmless if left). DEDUP key reverts to
`received_at`. No data migration beyond the additive column. `git revert`-clean.

## Observability

- No new error code. The fingerprint is internal; existing tick metrics unchanged. The 15:31 Dhan
  1m cross-verify remains the end-to-end completeness detector.

## ⚠️ Deploy window

This migrates the LIVE `ticks` table (adds a column + changes the DEDUP key). **Deploy AFTER market
close (≥15:30 IST)** — the brownfield ADD-COLUMN-before-DEDUP ordering makes it safe, but a schema
change on the actively-written table is best done off-session.

## Per-item guarantee matrix

All 15 "100% everything" rows + 7 resilience rows from
`.claude/rules/project/per-wave-guarantee-matrix.md` apply. O(1) per-tick hash (zero-alloc), no
hot-path allocation, composite-key uniqueness strengthened, replay-idempotent, ratcheted by the 5
new hash tests + the updated dedup-key test + the dedup_segment_meta_guard.

## Plan Items

- [x] Add deterministic `tick_payload_hash` (FNV-1a, fixed seed)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_payload_hash_is_deterministic_across_fresh_ticks, _ignores_received_at_replay_safe, _differs_on_distinct_price, _differs_on_distinct_volume, _differs_on_exchange_timestamp
- [x] Swap DEDUP key received_at → payload_hash + DDL + brownfield ADD COLUMN before DEDUP
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_tick_dedup_key_includes_segment, dedup_segment_meta_guard
- [x] Sync the data-integrity rule to the new key + determinism/replay rationale
  - Files: .claude/rules/project/data-integrity.md
  - Tests: n/a (docs)
