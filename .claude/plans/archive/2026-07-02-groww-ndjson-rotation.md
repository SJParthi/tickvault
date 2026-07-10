# Implementation Plan: Groww capture-file rotation + persisted bridge offset (PR-3)

**Status:** VERIFIED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — standing fix-sequence directive this session ("once it gets merged go ahead with the plan always"); PR-3 of the adversarial-sweep queue, started on #1307's merge.

## Design

**Problem A — unbounded growth:** the sidecar appends every tick to
`data/groww/live-ticks.ndjson` FOREVER. At ~767 SIDs streaming all day this is
a slow-motion disk-full on the 30 GB gp3 volume, and (hostile-agent LOW) a
respawned bridge re-tails the whole multi-day file at 4 MiB/wake — minutes of
candle lag after a mid-day respawn.

**Problem B — byte-0 re-tail on every boot:** the bridge's read offset is
in-memory only; every process restart re-reads the entire file. DEDUP keeps it
residual-neutral, but it wastes boot time and QuestDB write pressure.

**Fix A — IST-midnight rotation in the SIDECAR (the writer owns rotation):**
at the first emit after IST midnight (pure day-boundary check on the wall
clock, mirroring the bridge's force-seal boundary), the sidecar closes the
current file, renames it to `live-ticks-YYYYMMDD.ndjson` (the COMPLETED IST
day), reopens a fresh `live-ticks.ndjson`, and deletes dated archives older
than `NDJSON_ARCHIVE_KEEP_DAYS = 2` (the rows are in QuestDB, cross-verified
at 15:31 and conservation-audited at 15:40 daily — the archive is only a
crash-recovery window). The bridge already handles the swap: its existing
shrink detection (`len < offset` → reset to 0) picks up the fresh file.
**Why midnight (not size-based):** rotation while ticks flow could reset the
bridge's per-file `capture_seq` and collide DEDUP keys `(ts, sid, segment,
capture_seq, feed)` for same-second ticks; at IST midnight the market is
closed and timestamps differ across the boundary, so a `capture_seq` restart
can never collide. Size-based emergency rotation is deliberately NOT added
(honest bound: one day of Groww NDJSON ≈ low GB worst case, fits the volume;
the wipe buttons + retention sweep own the pathological cases).

**Fix B — persisted flushed-offset in the BRIDGE:** after each flush `Ok`,
the bridge persists `{offset, capture_seq, file_len}` to
`data/groww/bridge-offset.json` (atomic tmp+rename, throttled to ≥5s between
writes — cold path). On start it resumes from the persisted offset ONLY when
the identity checks pass (persisted offset ≤ current file length AND the
persisted `capture_seq` matches the line count consumed — i.e. the file was
not rotated/replaced under it); otherwise fail-safe byte-0 re-tail
(DEDUP-idempotent, exactly today's behavior). The offset is flushed-through
(never ahead of persisted rows), so a crash between parse and flush replays
only the unflushed tail — zero loss, bounded replay.

**Wipe interplay:** the dashboard wipe removes `data/groww/` entirely —
offset + archives + live file all go together (already shipped in #1307).

## Edge Cases
- Rotation exactly at midnight with the sidecar mid-reconnect → rotation
  happens on the next EMIT (writer-side), so no partial line is ever split.
- Bridge wakes during rename (file briefly absent) → read fails soft → next
  wake finds the fresh file, shrink logic resets to 0.
- Persisted offset > file length (rotated while bridge down) → identity check
  fails → byte-0 re-tail of the FRESH (small) file.
- Corrupt/missing offset file → parse fails soft → byte-0 re-tail.
- Archive delete failure (permissions) → warn + continue (retention re-tries
  next midnight); never blocks capture.
- Two days of downtime → bridge resumes on the live file; the archived days
  are already in QuestDB (they were flushed before the downtime) or replayed
  from the live file only.
- Wipe during the day → dir recreated by the sidecar on next emit; offset
  gone → byte-0 of an empty file. Clean.

## Failure Modes
- Offset write failure (disk) → warn + counter; bridge continues with
  in-memory offset (behavior = today's).
- Rotation rename failure → sidecar keeps appending to the current file
  (capture NEVER stops for a rotation problem) + prints one edge-triggered
  error line (supervisor routes it).
- A future edit persisting the offset BEFORE flush (data-loss ordering bug) →
  source-scan ratchet pins the persist call inside/after the flush-Ok arm.

## Test Plan
- Sidecar pure fns: `_ist_day(ts)`, `_should_rotate(prev_day, now_day)`,
  archive-name builder, retention selector (which files to delete) — selftest
  extended.
- Bridge pure fns: offset snapshot serialize/parse round-trip, resume
  decision (`resume_offset(persisted, file_len)` matrix: match / rotated /
  corrupt / missing), throttle gate.
- Source-scan ratchets: persist-after-flush ordering; sidecar rotation gated
  on the IST day boundary (not size); retention constant pinned.
- `cargo test -p tickvault-app` + guard suites green; python ast + selftest.

## Rollback
`git revert` — no schema change; the offset file is ignored by old code and
the dated archives are inert extra files.

## Observability
- Sidecar prints one line per rotation (`groww capture rotated: <archive> …`)
  and per retention delete; bridge logs offset-resume vs byte-0 decisions at
  info with the reason; `tv_groww_bridge_offset_resume_total{outcome}` counter.

## Plan Items
- [x] Sidecar: IST-midnight rotation + 2-day archive retention + selftest
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: test_sidecar_rotates_at_ist_midnight (ratchet)
- [x] Bridge: persisted flushed-offset + identity-checked resume
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_offset_snapshot_roundtrip, test_resume_from_snapshot_decision_matrix
- [x] Ratchets: persist-after-flush ordering + rotation-boundary pins
  - Files: crates/common/tests/groww_no_mint_guard.rs (extend) or crates/app/src/groww_bridge.rs inline
  - Tests: test_offset_persist_is_after_flush_ok

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Normal midnight | file rotated to dated archive, fresh file, bridge resets cleanly, archives >2 days deleted |
| 2 | App restart mid-day | bridge resumes from persisted offset — no full re-tail, boot fast |
| 3 | Bridge crash mid-burst | respawn resumes from last FLUSHED offset; unflushed tail replays; DEDUP collapses |
| 4 | Offset file corrupt/rotated-under | byte-0 re-tail (today's safe behavior) |
| 5 | Rotation rename fails | capture continues uninterrupted + one error line |
