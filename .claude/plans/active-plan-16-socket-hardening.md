# Implementation Plan: 16-socket seam hardening (allocation gate, flush stall, depth budget)

**Status:** IN_PROGRESS
**Date:** 2026-08-14
**Approved by:** coordinator-relayed session briefing, 2026-08-14 — recorded honestly:
this is NOT a fresh dated operator quote. The scope authority for the lane itself is the
existing 2026-08-09 / 2026-08-11 / 2026-08-12 dated quotes in
`websocket-connection-scope-lock.md`. This plan adds NO new scope: it does not open a
socket, widen a universe, touch `dry_run`, or edit the §28 frozen area. It hardens code
that PR #1749 already landed.
**Guarantee matrices:** carried by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row).
**Crates touched:** `crates/app`, `crates/core`, `crates/storage`, `crates/common`.

## Context — why now

PR #1749 (merged 2026-08-14 05:17 UTC) repaired the dark main feed and widened the live
universe from 4 instruments to ~4,565. The lane is designed for 16 sockets. That change
multiplied the traffic through a seam whose allocation behaviour has **never been
gated**, and whose back-pressure recovery path does not exist. Two of the four items
below are carried forward from `2026-08-10-dhan-main-feed-transport.md`, which recorded
them as shipped-by-omission; they were not.

## Design

**Item 1 — DHAT allocation gate on the 16-socket seam (carried from the archived plan's
Item 6, widened).** Six functions carry every frame from socket to database and none is
covered by any `dhat_*.rs` test (verified: zero matches for `WalRingSink`, `try_reserve`,
`run_frame_drain`, `drain_main_feed_frame`, `ingest_tick_at`, `append_tick_with_seq`).
The new gate drives the REAL types — a real `WsFrameSpill`, a real `RingByteBudget`, a
real `WalRingSink` — over 10k frames and asserts steady-state zero allocation. It must
not repeat the trap in `dhat_ws_reader_zero_alloc.rs:70-76`, which simulates the tail
with a locally-constructed `Bytes` clone and therefore cannot observe a regression in
the sink it claims to cover.

**Item 2 — re-fold WAL frames after a blocking ILP flush stall.** `blocking_flush` at
`dhan_feed_stack.rs:1490/1500/1624` runs inside `run_frame_drain`, so `rx.recv()` stops
for up to the 5 s ILP timeout. Frames pile into the single 65,536-slot mpsc; past it
`WalRingSink::accept` returns `RingFull` and the frame is WAL-only. `:640-659` states
there is no re-fold path, so a database stall becomes permanent tick loss. The
`block_in_place` choice is deliberate and documented at `:1394` (it preserves effective
worker count and the closure borrows `&mut ingest`) — this item does **not** change the
mechanism. It builds the RECOVERY: the bytes are already durable in the WAL segment, so
re-fold from there, DEDUP-idempotent via the replay-stable `capture_seq`.

**Item 3 — give depth frames their own ring byte budget.** `:1474-1482` counts depth
frames as `depth_unconsumed` and discards them — but only AFTER they have taken a ring
slot and shared `RingByteBudget` bytes. depth-200 admits frames up to 512 KiB against a
main-feed cap of 256 KiB, so unconsumed depth can evict the main feed from its own
budget. A separate budget removes the coupling.

**Item 4 — remove the per-drop heap allocation in the WAL drop path.**
`ws_frame_spill.rs:374-390` (and the Disconnected arm at `:409`) use labelled
`metrics::counter!`; a keyed `Key` owns a `Vec<Label>`, so this heap-allocates once per
DROPPED frame — on the one path that executes only when the process is already losing
data. Pre-resolve the handles at construction, the pattern already used at
`pool_supervisor.rs:971-1008`.

**Item 5 — capture-at-receipt ordering guard (carried from the archived plan's Item 5).**
A source-order guard asserting the WAL append precedes any parse/broadcast. This is the
durability invariant the whole chain rests on and it is currently unpinned.

## Edge Cases

- Frame larger than the whole byte budget — `try_reserve` refuses by construction; the
  gate must not treat that refusal as an allocation event.
- Re-fold racing a live frame for the same `capture_seq`: must collapse, not duplicate.
  `capture_seq` is replay-stable and read back from the `TVW2` WAL record rather than
  re-stamped, which is what makes this idempotent.
- Re-fold triggered while the ring is STILL full — must not recurse into a second stall.
- Depth budget exhausted while the main-feed budget is healthy — main feed must be
  unaffected (that is the whole point of the split).
- `depth_unconsumed` frames with zero depth instruments subscribed (today's real state):
  the split budget must not change behaviour when depth is empty.
- DHAT under `cargo llvm-cov` — coverage instrumentation perturbs allocation counts;
  `dhat_*` tests are already skipped under coverage and this one inherits that.
- Counter split must not double-count: a frame is refolded XOR lost, never both.

## Failure Modes

| Failure | Detection | Response |
|---|---|---|
| Allocation regression on the seam | Item 1 DHAT gate fails the build | PR blocked |
| ILP stall → ring full → WAL-only | `tv_dhan_feed_ring_full_total{outcome}` | re-fold; `lost` only if re-fold itself fails |
| Re-fold fails (WAL segment unreadable) | `outcome="lost"` increments | loud coded `error!`, frame counted lost — never silently green |
| Depth floods the shared budget | previously invisible | now bounded to the depth budget; main feed unaffected |
| Allocator pressure during data loss | Item 4 removes it | pre-resolved handles |
| Capture/parse order inverted by a future edit | Item 5 source guard | build fails |

## Test Plan

- `crates/app/tests/dhat_socket_seam_zero_alloc.rs` — the Item 1 gate, real types, 10k
  frames. Three-state proof required in the PR body: clean GREEN, planted allocation
  RED, reverted GREEN. A gate that has not been shown to fail is not a gate.
- Re-fold: unit tests for idempotency (same `capture_seq` twice → one row), for
  re-fold-while-still-full, and for the unreadable-segment path.
- Depth budget: main-feed admission unaffected while the depth budget is exhausted.
- Item 4: covered by the Item 1 gate driving the drop path (the allocation it removes is
  exactly what the gate would catch).
- Item 5: source-order scan.
- Scope: `cargo test -p tickvault-app -p tickvault-core -p tickvault-storage`;
  `crates/common` changes escalate to `--workspace` per `testing-scope.md`.

## Rollback

Every item is additive and independently revertable:
- Item 1 and Item 5 are test-only — reverting removes a gate, never behaviour.
- Item 3 is a constant plus a second budget handle; reverting restores the shared budget.
- Item 4 is a mechanical refactor with identical metric names and label sets, so a
  revert is invisible to dashboards and alarms.
- Item 2 is the only behavioural change. It is guarded so that a re-fold failure
  degrades to exactly today's behaviour (frame WAL-only, counted lost) rather than to
  something worse.
- No schema migration, no DEDUP-key change, no config default flip.

## Observability

- `tv_dhan_feed_ring_full_total` gains an `outcome` label split — `refolded` vs `lost` —
  so Item 2 is measurable rather than asserted. Without the split, a successful re-fold
  and a permanent loss are the same number.
- Every new counter is added to the EMF selector **in this same PR**.
  `loss_counter_visibility_guard` exists precisely because a counter that is never
  shipped is invisible loss, and shipping the counter in a later PR would reproduce that
  failure.
- Pre-registration at zero for any new series, per the CloudWatch first-sample-delta
  discipline already documented on `WalRingSink::pre_register`.
- No new CloudWatch alarm in this plan: these series have no sustained baseline yet, and
  an alarm on a metric with no history is noise.

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: the seam's
steady-state allocation behaviour becomes build-gated, a database stall stops being
permanent tick loss inside the WAL's retention, and depth can no longer evict main-feed
budget.

**NOT claimed:** that this makes the feed work. As of this plan the lane has never been
observed receiving a tick — the 2026-08-12 cross-verification reported `compared: 0`
with `missing_live: 373`, and no session has yet reported a non-zero `compared`.
**NOT claimed:** that it addresses the 2026-07-13 retirement reasons (p99 delivery lag
46.37 s, 29–67 silent instruments/minute) — every one of those is Dhan-side and none is
touched here. **NOT claimed:** any measurement at the ~4,565-instrument scale; the
allocation gate is a unit-level proof, not a load test.

---

## ITEM 6 (added 2026-08-14, operator: "i want all 16 wecoekst shdu lwork") — depth late-attach

**This is THE change that opens the 10 dark depth sockets.** Everything else in this
plan hardens a lane that is running; this one changes how many sockets carry data.

### Root cause (Verified, file:line)

`load_depth_universe` is called ONCE at `main.rs:2028`, during boot (~08:30 IST). Its
only data source is the `option_chain_1m` table, which the option-chain leg does not
populate until its first fire at **09:16 IST**. Depth therefore asks for its instrument
set ~45 minutes before that set exists.

`build_depth_candidate_query` (`dhan_depth_universe.rs:374`) additionally has **no day
bound on `ts`** — `LATEST ON ts PARTITION BY ...` returns the newest row per partition
from ANY day. Two distinct daily failures follow:

| Morning | Behaviour |
|---|---|
| Normal | Returns YESTERDAY's rows. Depth opens, but ranks ATM off a stale `underlying_spot`, on contract ids Dhan documents as unstable across days. |
| After expiry | `expiry >= today` filters everything out → **zero depth sockets**, and `dhan_depth_universe.rs:527-537` prescribes a manual restart — which violates the zero-manual-intervention mandate. |

### Why the one-line fix is REJECTED

Adding `AND ts >= {today_micros}` alone is *correct* but strictly reduces socket count:
at 08:30 the day-bounded query returns zero rows, so depth would open 0 sockets instead
of 10-on-stale-data. The day bound MUST land together with late-attach, never before it.

### The change

1. **Split the dial flow in `run_dhan_feed_stack`.** Plan + dial `MainFeed` immediately
   (it must be live at 09:00 — delaying it to 09:16 loses the first minute of ticks,
   which is real, unrecoverable loss). Then, in the SAME task, await the depth universe
   and dial `Depth20`/`Depth200` when it arrives. The pool must survive across that
   await; `FEED_STACK_SPAWNED` forbids a second stack, so this cannot be bolted on from
   outside the function.
2. **Move the depth-universe load off boot** into that task, as a bounded retry:
   re-query every 60 s from 09:16 IST until non-empty or a 10:00 IST deadline.
   Fail-LOUD and fail-closed at the deadline (coded `error!` + counter), never a silent
   zero-socket session.
3. **Add the day bound** `AND ts >= {today_micros}` — safe only once (1) and (2) land.
4. **Re-select on expiry roll**: because the set is now acquired after the chain leg
   runs, the post-expiry morning resolves itself with no restart.

### Test plan

- Empty-at-boot → non-empty-at-09:16 opens depth sockets without a restart (the whole
  point; assert socket count goes 0 → N).
- Main feed dials at 09:00 and is NOT delayed by the depth wait (assert ordering) — this
  is the regression that would cost real ticks if got wrong.
- Deadline path with a permanently empty chain: zero depth sockets, coded error, counter,
  main feed unaffected.
- Day-bounded query returns nothing for yesterday-only rows.
- `FEED_STACK_SPAWNED` still refuses a duplicate stack.

### Honest risk

This touches the live-feed spawn lifecycle. A half-done version takes the 5 WORKING
main-feed sockets down with it. It must not be attempted without room to run
`cargo test -p tickvault-app` and the wiring guards to completion.

## ITEM 7 — `tv_dhan_feed_stack_up` is set on SPAWN, not on CONNECT

`dhan_feed_stack.rs:2216` sets the gauge to 1 after the dial loop *spawns tasks*, and it
clears only when the drain loop breaks. `DialFailed`/`Transient` never park, so on a
total dial outage all sockets loop forever holding their senders and the gauge reads 1
with zero data — the same false-OK class as the boot-time connections constant.

Even after Item 6 opens depth, this gauge cannot confirm it. Flip it to set on first
successful frame per endpoint, not on spawn.
