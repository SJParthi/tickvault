# Implementation Plan: WAL replay crash-safety — un-confirmed segments must re-replay

**Status:** APPROVED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — grounded directive this session: close the
audit's zero-loss MEDIUM in `crates/storage/src/ws_frame_spill.rs` (replay archives
segments BEFORE the re-injected frames are confirmed persisted, and never re-globs
`archive/`, so a second crash strands those frames recoverable-by-hand-only).

> Guarantee matrices: this item carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (mandatory per `per-item-guarantee-check.sh`). The dominant guarantee for this
> change is the zero-tick-loss envelope: WAL → ring → spill → DLQ is the lossless
> floor; this fix makes the boot-replay leg of that floor crash-safe.

Crate touched: **`tickvault-storage`** (`crates/storage/src/ws_frame_spill.rs`) plus
the three storage-crate tests that encode the OLD (buggy) archive-on-replay contract,
plus a one-line confirm wiring in each of the two `crates/app/src/main.rs`
(`tickvault-app`) boot paths that call `replay_all`.

## The confirmed bug (file:line evidence)

`crates/storage/src/ws_frame_spill.rs::replay_all` (lines 812–868 on `origin/main`):
1. L818–822 globs ONLY `*.wal` in `wal_dir` — never `archive/`.
2. L828–836 reads each segment into the returned `frames` Vec (the caller will async
   re-inject these into the live pipeline far downstream).
3. L857–865 IMMEDIATELY renames every segment → `archive/` — comment L857 literally
   says "Archive processed segments so we don't replay them twice" — BEFORE the
   caller's re-injection/persist is confirmed.

Caller (`crates/app/src/main.rs`): `replay_all` runs at L670; the recovered frames are
re-injected into the live pool mpsc at L1486 (fast boot) / L5522 (slow boot) — ~800+
lines later, async, with NO persist-confirmation signal back to the spill layer. The
existing code's own re-inject comment calls a failed re-inject "a degraded mode, not a
data loss (the frames are still on disk)" — but the on-disk segment is in `archive/`,
which `replay_all` NEVER re-globs, so on the NEXT boot it is silently NOT re-injected.

Crash sequence that loses auto-recovery: crash → boot → `replay_all` returns frames +
archives segment → app crashes again before the re-injected frames are persisted →
next boot globs only `*.wal` → archived-but-unpersisted frames are stranded in
`archive/`, recoverable by hand but never auto-replayed.

## Design

Chosen approach: **Option A — staging dir (`replaying/`), re-globbed on boot.**

Why Option A over Option B (persist-before-archive): Option B needs a persist-
confirmation signal flowing from the live pipeline (mpsc → tick processor → ring →
spill → WAL → DB, ~800 lines and an async boundary away) back to the spill layer.
No such signal exists today; adding it would be a large, cross-cutting change across
`main.rs` and the tick processor — high blast radius on the hot path. The existing
code structure has a clean, natural confirmation point: the boot re-injection loop
(`sender.try_send(frame)` succeeded → the frame is now back inside the live
ring→spill→WAL chain and durably re-captured into a NEW segment). Option A needs only
a small, contained, cold-path change in the storage crate plus a one-line confirm call
at that natural point. Option A is the operator-recommended safer minimal fix.

New semantics of `replay_all(wal_dir)`:
1. Re-glob `<wal_dir>/replaying/*.wal` (un-confirmed leftovers from a prior crash —
   small + bounded: at most the segments a single crashed boot was draining) AND
   `<wal_dir>/*.wal` (fresh-crash segments). Sort the COMBINED set lexicographically by
   the zero-padded-nanos filename so cross-source append/chronological order is
   preserved (replaying-dir leftovers were created BEFORE this boot's fresh segments,
   so their nanos are smaller → they sort first → correct order).
2. Read every segment (CRC-validated, partial-tail-safe — unchanged `replay_segment`).
3. Move every read segment → `<wal_dir>/replaying/` (NOT `archive/`). A name collision
   in `replaying/` (a leftover re-read this boot) is overwritten by rename — same bytes,
   safe.
4. Return the recovered frames.

New `confirm_replayed(wal_dir)`: moves every `<wal_dir>/replaying/*` → `<wal_dir>/archive/`.
Called by the caller AFTER the recovered frames have been successfully re-enqueued into
the live pipeline (i.e. durably re-captured). Until confirmed, the segment stays in
`replaying/` and is re-replayed on the next boot.

Conservation count (`count_frames_for_ist_day`): add `<wal_dir>/replaying/` to the
existing `[wal_dir, archive]` scan set so an un-confirmed segment's frames are still
day-attributed (no silent under-count). Still READ-ONLY.

Wiring in `main.rs` (one call per boot path, at the natural confirmation point): after
the re-injection loop completes WITHOUT a `Closed`/`Full` drop AND with the pool ready
(frames durably back in the chain), call `confirm_replayed(&ws_wal_path)`. If
re-injection dropped frames or the pool wasn't ready, DO NOT confirm — the segment
stays in `replaying/` for next boot.

## Edge Cases

- Empty / missing `wal_dir`: unchanged — `replay_all` returns `Ok(vec![])` (re-glob of a
  non-existent `replaying/` yields nothing; `confirm_replayed` on a missing `replaying/`
  is a no-op).
- No `replaying/` dir (normal first boot): re-glob yields nothing; identical to today.
- Leftover in `replaying/` + fresh `*.wal`: BOTH replayed, combined-sorted by nanos
  filename → strict append order preserved across both sources.
- Same segment name already in `replaying/` (re-read this boot): rename overwrites with
  identical bytes — safe, no loss.
- `confirm_replayed` never called (crash before confirm, or re-inject dropped): segment
  stays in `replaying/` → re-replayed next boot → DEDUP `(ts, security_id, segment,
  capture_seq, feed)` collapses any frames that DID get persisted (capture_seq is
  replay-stable, read back from the TVW2 record — idempotent).
- Corrupted/truncated segment in `replaying/` or `*.wal`: `replay_segment` stops cleanly
  at the boundary (unchanged), segment still moved to `replaying/` so it is not lost.

## Failure Modes

- Disk full when creating `replaying/` or renaming into it: `replay_all` uses
  best-effort `create_dir_all`/`rename` wrapped in `drop(...)` (same pattern as the
  current archive code) — a failed move leaves the segment as `*.wal`, which is STILL
  re-globbed next boot (strictly safe — never worse than today).
- `confirm_replayed` rename fails (disk full): segment stays in `replaying/` → harmless
  re-replay next boot (DEDUP-idempotent). Logged + counted, never panics, never blocks.
- Unbounded `replaying/` growth (the regression to avoid): bounded because (a) only
  segments a crashed boot was actively draining ever land there, and (b) every healthy
  boot `confirm_replayed`s them to `archive/`. `archive/` is NEVER re-globbed, so the
  confirmed-history accumulation does NOT cause re-replay (the whole-archive-re-replay
  regression is explicitly avoided).
- Re-replay duplication: prevented by the existing replay-stable `capture_seq` DEDUP key
  — a re-replayed frame that was already persisted collapses to the same row.
- Ordering regression: combined `replaying/ + *.wal` set is sorted by the same
  zero-padded-nanos filename key the order guard relies on; leftovers (older nanos) sort
  before fresh segments → FIFO preserved.

## Test Plan

In `crates/storage/src/ws_frame_spill.rs` (`#[cfg(test)]`) — NEW:
- `test_replay_moves_to_replaying_not_archive_until_confirmed` — after `replay_all`,
  segment is in `replaying/`, NOT `archive/`; confirm moves it to `archive/`.
- `test_unconfirmed_segment_is_rereplayed_on_next_boot` (TEST CASE 1) — `replay_all`
  twice with NO `confirm_replayed` between → second call RE-RETURNS the same frames.
- `test_confirmed_segment_is_not_rereplayed` (TEST CASE 2) — `replay_all` →
  `confirm_replayed` → `replay_all` again returns 0; a third boot also 0 (archive never
  re-globbed).
- `test_replaying_leftover_and_fresh_replay_in_order` (TEST CASE 3) — leftover in
  `replaying/` (older nanos) + fresh `*.wal` (newer nanos) → both replay, leftover first.
- `test_crash_between_move_and_confirm_still_rereplays` (TEST CASE 4) — `replay_all`
  (segment now in `replaying/`), simulate crash (NO confirm) → fresh `replay_all`
  re-returns it.
- `test_confirm_replayed_missing_dir_is_noop` — `confirm_replayed` on a dir with no
  `replaying/` does not error.
- `test_count_frames_scans_replaying_dir` — conservation count includes a segment in
  `replaying/`.

UPDATED (these encode the OLD archive-on-replay contract; switch to the new confirm
idiom — proving TEST CASE 2):
- `ws_frame_spill.rs::test_append_spill_and_replay_roundtrip` — `confirm_replayed`
  between the two `replay_all` calls before asserting the second is empty.
- `crates/storage/tests/chaos_ws_frame_wal_replay.rs` (P7 Scenario 5) — assert un-confirmed
  re-replays, then confirm, then assert no re-replay + archive populated.
- `crates/storage/tests/zero_tick_loss_sla_guard.rs` Phase 3 — assert un-confirmed
  re-replays, then confirm, then assert empty.

Run `cargo test -p tickvault-storage` (scoped per testing-scope.md — storage-only source
change). Run banned-pattern-scanner, plan-gate (expect PASS), plan-verify.

## Rollback

Single, self-contained storage-crate change plus a one-line confirm call per boot path.
Rollback = `git revert` the PR: `replay_all` reverts to archive-on-replay, `main.rs`
drops the two `confirm_replayed` calls. No schema change, no migration, no on-disk format
change (segments are the same TVW2 files; only WHICH directory they sit in differs).
A box upgraded to the new code that rolls back: any segment left in `replaying/` is simply
never re-globbed by the old `replay_all` — i.e. rollback degrades to exactly today's
behaviour (recoverable-by-hand), never worse.

## Observability

- `replay_all` keeps its existing `tv_wal_replay_recovered_total` /
  `tv_wal_replay_corrupted_segments_total` counters + the "WAL replay complete" info log;
  add the `replaying/` leftover count to the structured log so an operator sees when a
  prior crash left un-confirmed segments.
- `confirm_replayed` increments a new `tv_wal_replay_confirmed_segments_total` counter
  and logs an info line; a rename failure logs `error!` (never panics) so a
  stuck-in-`replaying/` segment is visible.
- No new ErrorCode variant needed — the existing WS-SPILL-01/02 + the
  `tv_ws_frame_wal_reinjected*` counters already cover the re-injection leg; this change
  only makes the archive timing crash-safe and adds a confirm counter.
- 7-layer note (honest): this is a cold-path boot function, so the layers that apply are
  Prom counter + tracing log + the storage-crate ratchet tests; no Telegram/audit-table
  row is warranted for a routine boot-replay confirmation (a stuck segment surfaces via
  the existing zero-tick-loss SLA counters + the daily conservation audit which now scans
  `replaying/`).
