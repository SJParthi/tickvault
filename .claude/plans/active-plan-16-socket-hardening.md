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

### Item 6 — implementation hazards found 2026-08-14 by tracing the real code

Three details decide whether this change works or silently kills the main feed.
Found by reading `run_dhan_feed_stack` end to end; recorded so they are not
rediscovered the hard way.

**H-A. `params.questdb` is ALREADY on `DhanFeedStackParams` (`:1868`).** An earlier
estimate in this session claimed a new params field plus a call-site cascade was
needed. That was WRONG. `run_dhan_feed_stack` can call `load_depth_universe`
itself with no signature change. The change is materially smaller than first scoped.

**H-B. `drop(frame_tx)` at `:2195` is load-bearing and must NOT be delayed.** The
template sender is dropped immediately after the dial loop precisely so the ring
can close when the last socket dies — otherwise the drain hangs forever instead of
reporting the lane went dark. A depth phase inserted inline BEFORE that drop would
hold the template alive for the whole ~45-minute wait, so a total socket failure in
that window would look like a live lane instead of a dead one. The depth attach
MUST therefore own its own `frame_tx.clone()` and run as a SPAWNED task, leaving
the `:2195` drop exactly where it is.

**H-C. `pool` ownership.** The dial loop takes supervisors out of `pool` by index
(`core::mem::replace`, `:2138-2142`). A spawned depth task needs `&mut pool` too,
so `pool` must be MOVED into that task (verify no later use in
`run_dhan_feed_stack` first) rather than borrowed across the await.

**Consequence for sequencing:** the depth phase goes AFTER the main dial loop and
AFTER `drop(frame_tx)`, as a spawned task owning `(pool, frame_tx.clone(), spill,
ring_budget, client_id, token)`. `drain` is already a spawned task (`:2123`), so the
main feed and the fold are entirely unaffected by however long depth waits.

**Dial-body extraction:** the loop body (`:2126-2191`, ~65 lines) must become a
reusable fn — it is needed for both phases, and duplicating it would let the two
copies drift on token refresh, which is the one behaviour that must be identical.

**The single test that matters most:** assert the main feed dials and the drain is
consuming BEFORE the depth universe resolves. If that ordering inverts, the change
trades 5 working sockets for 0 during market hours — the exact failure this plan
exists to avoid.

**H-D. The sender-clone vs ring-close tension — FOUND AND RESOLVED 2026-08-14.**

H-B says the depth task must own a `frame_tx` clone (so the `:2195` drop is not
delayed). But a held clone keeps the ring OPEN for the entire ~45-minute wait, so if
every main-feed socket died in that window the drain could not close and the lane
would look alive while producing nothing — reintroducing, from a new direction, the
exact false-OK this plan exists to remove. H-B and the ring-close invariant are in
direct tension, and the plan as first written did not resolve it.

**Resolution: the depth task holds a `tokio::sync::mpsc::WeakSender`, not a
`Sender`.** Verified against the pinned dependency, not assumed —
`tokio-1.53.1/src/sync/mpsc/bounded.rs`: `WeakSender<T>` (`:56`),
`Sender::downgrade()` (`:1576`), `WeakSender::upgrade() -> Option<Sender<T>>`
(`:1665`).

Shape:
- main flow: `let depth_weak = frame_tx.downgrade();` then `drop(frame_tx)` stays
  EXACTLY where it is at `:2195` — a weak handle does not keep the channel open.
- depth task, once the universe resolves: `let Some(tx) = depth_weak.upgrade() else
  { /* ring already closed — the lane went dark while we waited; log coded and
  return */ };` then build the `WalRingSink` from `tx` and dial.

This is strictly better than a bare clone: it not only preserves the close
invariant, it gives depth a CORRECT answer when the lane died during the wait —
it declines to dial into a dead ring instead of opening sockets that feed nothing.

With H-D resolved, the remaining work is mechanical: extract the dial body
(`:2126-2191`) into a reusable fn, move `pool` into the spawned depth task
(confirmed no use after the loop), and add the bounded retry loop around
`load_depth_universe(&params.questdb, ist_midnight_nanos(today))`.

---

## ITEMS 8–14 (added 2026-08-14) — operator: "Go ahead with the entire fixes"

**The verbatim operator authorization (2026-08-14, typed directly in-session — preserve
EXACTLY, typos included):**

> "Go ahead with the entire fixes dude okay? Not per Sid cloudwatch right per websocket
> connections or entire webscoket connections right dude ami right dude"

Given in DIRECT response to a message that enumerated exactly eight fixes in a priority
table (drain respawn · token cooldown · stop the console lying + alarms · depth ring
budget · ILP over TCP · WAL re-fold · receipt-time latency · `target-cpu`). That table is
the scope this quote authorizes. The second sentence is a DESIGN CORRECTION the operator
made himself and it is adopted verbatim: latency is dimensioned **per WebSocket
connection (16)**, never per instrument (4,565).

**Why the correction is right, and recorded so nobody "improves" it back:** 4,565
per-instrument CloudWatch metrics ≈ $1,369/mo against a budget whose AUTOMATIC action is
`STOP_EC2_INSTANCES` — the observability feature would stop the trading box. 16
per-connection metrics ≈ $4.80/mo. Per-instrument drill-down stays in RAM, served over
the existing API, costing zero CloudWatch dimensions.

**This adds NO new scope.** No socket is opened beyond the 16 already authorized
(2026-08-09 second quote), no universe is widened, `dry_run` is untouched, and the §28
frozen area is not edited. Every item below repairs code that PR #1743–#1750 already
landed.

### Item 8 — the frame drain must be unkillable

`dhan_feed_stack.rs:2402-2416` awaits the drain join handle, logs one `error!`, sets the
gauge to 0, and **returns**. A panic in any drain arm therefore ends the only ring
consumer for the rest of the session: sockets keep capturing, the WAL keeps growing, and
zero rows reach `ticks` or `candles_*`. The silence detector cannot warn, because it
lives INSIDE the dead task. Fix: supervise the drain with the house respawn pattern
(`cadence_runner` precedent) — bounded restarts, `tv_dhan_feed_drain_respawn_total`,
coded `error!` per respawn, and a permanent-failure page after the cap.

### Item 9 — a fatally-parked socket must not be silent

`pool_supervisor.rs:1690` parks a socket permanently on a fatal disconnect (805) and never
dials again — deliberate, and correct as a dial policy. What is NOT correct is that
nothing tells the operator. Fix: `tv_dhan_ws_park_total` gains a CloudWatch alarm; the
park site emits a coded `error!` naming the endpoint and slot. The park policy itself is
UNCHANGED (re-dialling into a fatal reject is worse).

### Item 10 — the 807 token stampede

Every one of the 16 connection closures calls `manager.force_renewal()`
(`dhan_feed_stack.rs:2090-2103`). `try_renew_token` is never cooldown-gated, and the mint
guard is a **check-then-act across an `.await`** (read `token_manager.rs:960-965`, written
inside `acquire_token` at `:768-770`) — so two callers can both pass. Dhan permits ONE
active token per account, so mint *n* invalidates mint *n−1* and the sockets that already
re-dialled get 807 again. This path executes **every 24 h by regulation**, so it is not a
tail risk. Fix: a single-flight gate — the first caller mints, the other fifteen await the
same result; the cooldown stamp is written BEFORE the await, not after.

### Item 11 — stop the console lying

`set_dhan_lane_running` has ZERO production call sites (8 repo-wide hits, all tests), so
`feed_health.rs:162` returns `Degraded, "enabled, but the feed was not started at boot"`
**unconditionally** — whether the lane is healthy, dead, or never started. `feeds_page.rs`
then prescribes a restart from that constant. Separately, no `record_ticks` call site
exists, so Dhan feed health can never read `Down` — it returns a benign
`Unknown, "not instrumented yet"` for a corpse. Fix: call `set_dhan_lane_running(true)`
when the stack is actually up and `(false)` on every exit path; wire `record_ticks` from
the drain so health can fall.

### Item 12 — per-connection latency, receipt-stamped

Three defects make today's numbers unusable even if published: (a) the receive stamp is
taken in the DRAIN, after the shared 65,536-frame ring (`dhan_feed_stack.rs:1455`), so
under load it measures OUR queueing, not Dhan's delivery; (b) `CapturedFrame`
(`pool_supervisor.rs:832`) carries no slot, so a tick cannot be attributed to a socket;
(c) `DailyLagHistogram::record_ns` takes **unsigned** — one negative sample (host clock
behind exchange, or a garbage LTT) stores a ~570-million-year maximum permanently.

Fix: stamp receipt in the read task and carry it plus `slot: ConnectionId` on
`CapturedFrame` (a `u64` + a `u8` — no allocation; `Bytes` remains the only heap member).
Compute lag in `i64` on ONE explicit IST basis, clamp negatives to zero and COUNT the
clamp, reject `ts == 0` and `ts == u32::MAX` before recording. Publish p50/p99 per
connection (16 dimensions). Per-instrument stays in a RAM ring served over the API.

**Honest limits, stated at the point of display, not buried:** Dhan's LTT is whole
SECONDS, so every lag figure carries a ±1 s truncation floor and a mean +500 ms bias —
sub-second precision is structurally unavailable and must never be claimed. Only the 5
main-feed sockets produce ticks; depth sockets get a frame-CADENCE metric instead, and
the order-update socket is a separate JSON path. Idle instruments are excluded — LTT is
last-TRADE time, so a thin option is legitimately minutes stale and must never page.
Silence remains owned by `scan_silence`.

### Item 13 — alarms for a lane that currently has none

`grep NotificationEvent` over the feed stack returns EMPTY, and no `tv_dhan_*` metric
appears in any `deploy/aws/terraform/*.tf` alarm. Meanwhile `tv_ticks_dropped_total` is
already billed and shipped to CloudWatch with no alarm consuming it — the repo's own EMF
notes call it "the single largest tick-loss window". Fix: alarms on
`tv_dhan_feed_stack_up < 1`, `tv_ticks_dropped_total`, `tv_dhan_ws_park_total`, and the
ring-full counter. **A dated row lands in `dhan-rest-only-noise-lock-2026-07-14.md` §2
FIRST** — that file's §3 makes any new Dhan-scoped page a REJECT without one.

### Item 14 — build for the CPU we actually run on

`.cargo/config.toml` deliberately does not set `target-cpu`, and its stated reason is
portability between a Mac dev build and AWS. That reason is sound for the LOCAL profile
and does not apply to the deploy path, which cross-compiles for a KNOWN r8g.xlarge
(Graviton4 = `neoverse-v2`). Fix: set `-C target-cpu=neoverse-v2` in the deploy build
only, leaving the local build generic. The portability comment stays and gains the
carve-out so the next reader does not undo it.

### Items 8, 12, 14 — closure record (2026-08-18)

Verified in source on this branch before writing, not inferred from the item text above.

**Item 12(a) — DONE by this change.** The drain no longer opens with its own
`Utc::now()`. `CapturedFrame` carries `received_at`, a MONOTONIC instant stamped in the
read task (`WalRingSink::accept`, before the WAL append), and the drain derives the
receipt instant as `Utc::now() - frame.received_at.elapsed()`. A first attempt stamped a
WALL CLOCK in `pool_supervisor` and was correctly rejected by the existing ratchet
`test_pool_supervisor_source_never_reads_the_wall_clock` — that ban is load-bearing (an
NTP step must not be able to expire all sixteen sockets at once). The monotonic shape
respects it and is strictly better: a clock step landing between receipt and fold cancels
out of the difference instead of corrupting the sample. Guarded by
`dhan_feed_drain_consults_the_frames_receipt_stamp` and
`dhan_feed_drain_actually_subtracts_the_queue_delay` — the second exists because
computing `elapsed()` and then not applying it compiles, passes every unit test, and
silently restores the exact wrong measurement.

**Item 12(b) — already DONE by another route.** `CapturedFrame::connection_index: u8`
(`pool_supervisor.rs`) already carries the slot, and `record_ws_lag` already resolves it
to a per-slot histogram handle.

**Item 12(c) — the LIVE path is covered; the named symbol is dormant, not fixed.** Stated
precisely because this item names a specific symbol. The live per-connection path is
covered: `ws_lag_ms` returns `WsLag::ClampedNegative` for a negative sample (recorded as
0.0 and counted on `excluded_clamped_negative`) and `None` for an implausible LTT
(counted on `excluded_implausible_ltt`). The symbol the item actually names —
`DailyLagHistogram::record_ns` in `feed_lag_monitor.rs` — is STILL `u64` and is
UNCHANGED. It is unreachable rather than repaired: it is private and has ZERO production
producers (every call site is inside its own test module), which that file's own header
already records. Recorded this way deliberately — calling it "fixed" would be the
stale-row class this repo has had to correct repeatedly.

**Item 8 — WITHDRAWN, not deferred.** The proposed respawn supervisor would be
unreachable in production. The release profile sets `panic = "abort"` (`Cargo.toml:280`),
so a panic in any drain arm aborts the PROCESS — there is no unwind for a supervisor to
catch and no surviving task to respawn from. Recovery is already owned one layer up by
systemd `Restart=always` (`deploy/systemd/tickvault.service:94`), which restarts the whole
binary. Building the supervisor would add a respawn counter and a bounded-restart ladder
that can never increment, i.e. a permanently-green monitor — the exact false-OK class
Rule 11 forbids. The item's underlying concern (a dead drain must be visible) is real and
is served by `tv_dhan_feed_stack_up < 1`, already alarmed under item 13.

**Item 14 — already DONE, and deliberately `neoverse-n1`, not `-v2`.**
`.cargo/config.toml:58` sets `rustflags = ["-C", "target-cpu=neoverse-n1"]` on the deploy
target. The item text above proposes `neoverse-v2` (Graviton4). That is NOT an oversight
to correct: the operator-sanctioned rollback path from r8g.xlarge runs t4g (Graviton2 =
`neoverse-n1`), and a binary built for V2 would fault with SIGILL on a t4g box the moment
that rollback was exercised. `n1` runs correctly on both, so it is the correct choice for
a target set that includes the rollback host. Left as-is.

## Item 8–14 Edge Cases

- Drain respawn must NOT restart into a poison frame forever — the restart budget is
  bounded and exhaustion pages rather than looping.
- The single-flight token gate must not deadlock when the first caller panics — the
  permit is released on drop, not on success.
- `set_dhan_lane_running(false)` must fire on EVERY exit path including panic-unwind,
  or the console flips from one lie to the opposite lie.
- The lag histogram must reject a frame stamped before its own connection opened (clock
  step during dial).
- A depth socket must never contribute to the main-feed lag percentiles.

## Item 8–14 Failure Modes

| Mode | Detection | Recovery |
|---|---|---|
| Drain panics repeatedly | respawn counter + cap | page after cap, gauge 0 |
| Token mint fails for all 16 | single-flight returns one error to all | existing backoff ladder |
| Clock steps backward mid-session | negative-lag clamp counter | series annotated, not silently zeroed |
| EMF name budget exceeded (16,382/16,384 bytes today) | terraform plan fails | short metric names, counted before adding |

## Item 8–14 Test Plan

Unit: single-flight gate under 16 concurrent callers (loom where practical); negative and
`u32::MAX` lag inputs clamp and count; `set_dhan_lane_running` toggles on both the success
and the panic path. Integration: drain respawn after an induced panic. Ratchet: a guard
asserting `set_dhan_lane_running` has ≥1 production call site — the defect class that
caused Item 11 in the first place. DHAT: the receipt stamp adds no allocation.

## Item 8–14 Rollback

Every item is independently revertable. Items 8–11 and 13 are additive (a supervisor, a
gate, a setter call, alarms) and removing them restores today's behaviour exactly. Item 12
adds two fields to an internal struct. Item 14 is one build flag.

## Item 8–14 Observability

New: `tv_dhan_feed_drain_respawn_total`, `tv_dhan_lag_negative_clamped_total`,
per-connection lag p50/p99 (16 dims). Existing-but-unwatched gain alarms:
`tv_dhan_feed_stack_up`, `tv_ticks_dropped_total`, `tv_dhan_ws_park_total`.

## Item 8–14 Honest envelope

100% inside the tested envelope: after these items nothing is lost after a frame reaches
our NIC, no failure is silent, and every stage of the chain is measured. NOT claimed:
(a) sub-second latency accuracy — Dhan's LTT is whole seconds and the ±1 s floor is
structural; (b) any improvement to Dhan's own delivery, measured 2026-07-06 at p99
46.37 s / max 198.69 s — every one of these fixes is on OUR side of the NIC and changes
that number by exactly zero; (c) detection of a tick Dhan never sent — their India feed
carries no sequence number and no snapshot-on-subscribe, so upstream loss stays invisible
at the protocol level; (d) that the lane has been exercised at 4,565 instruments — it has
not, on any day, and the first live session at that scale remains the measured gate.

---

## ITEMS 15–21 (added 2026-08-14) — operator: "Solve and fix all these also dude okay!"

Given in DIRECT response to the six-agent audit table naming these findings. No new
scope: no socket beyond the authorized 16, no universe widening, `dry_run` untouched,
§28 frozen area untouched. Every item repairs code already landed.

**15 — Frame cap sized off the wrong quantity (CRITICAL).**
`MAIN_FEED_MAX_FRAME_BYTES = 256 KiB`, justified in its own comment by a *subscribe
batch* of 100 instruments (~16 KiB). The subscribe batch has nothing to do with how many
packets Dhan coalesces into one WebSocket message. A socket carries up to 5,000
instruments; 5,000 × 162 B Full = 810 KiB. On overflow `Error::Capacity` maps to
`reason="oversize"` and returns `SocketEvent::Closed`, so the ladder reconnects,
resubscribes, and the burst repeats — a permanent reconnect loop precisely at 09:15.
Fix: size the cap from `instruments_per_connection × max_packet_bytes` with headroom,
and pin the derivation so it cannot drift from the subscribe-batch reasoning again.

**16 — The 16 second-scale timeframes are NOT gated (CRITICAL).**
`tf_index.rs` documents them as "GDF-feed-gated, zero rows until the GDF 1s live feed
lands". No gate exists — every tick path iterates `TfIndex::ALL` unconditionally, so a
Dhan tick opens AND seals S1..S30. At scale that is ~70 GB/day onto a 100 GB disk. Fix:
make the documented gate real, per feed, defaulting to the documented behaviour.

**17 — No shutdown teardown; the day's tail is lost silently (HIGH).**
The lane's handle is bound to `_dhan_feed_stack_monitor` and the shutdown path's Dhan
steps were "deleted with the lane" — the lane returned, the teardown did not. Nothing
closes the sockets, so the ring never closes, so the drain never breaks, so the final
`ingest.flush()` never runs. Sub-threshold ILP rows and open candle state evaporate while
the log prints "tickvault stopped" and classifies the shutdown clean. **This is losing
data today**, in Quote mode, at 4,565 instruments. Fix: signal the lane on shutdown and
flush before exit.

**18 — 804 misclassified as Transient (HIGH).** `classify_disconnect`'s catch-all sends
"instruments exceed limit" onto the infinite reconnect ladder, re-sending the identical
over-limit set every 30s forever. Fix: classify as Fatal → park (the park now pages).

**19 — 250 subscribe messages with zero pacing (HIGH).** Dhan documents no subscribe
rate limit, and 805's own text says "too many requests … may result in user being
blocked". We send the account's maximum possible subscribe volume as fast as the socket
accepts writes, on five sockets at once. Fix: a small inter-message delay and per-slot
connect stagger.

**20 — Seal ring evicts OLDEST (HIGH).** For a current-day store, drop-oldest destroys
the morning. Recorded; fix is a policy decision (drop-newest + loud counter) that needs
its own review, so it is FLAGGED here rather than silently changed.

**21 — QuestDB tuned for TCP 9009 while every writer uses HTTP 9000 (MED).** Five
`QDB_LINE_TCP_*` knobs tune a receiver nothing connects to; there is zero `line.http.*`
tuning. Fix: tune the transport actually in use, or move to ILP/TCP deliberately.

## Items 15–21 Edge Cases / Failure Modes / Test Plan / Rollback / Observability

Edge cases: a raised frame cap must not mask a genuinely runaway frame (keep a ceiling
and count refusals); the TF gate must not silently disable timeframes a feed legitimately
produces; shutdown flush must be bounded so a hung QuestDB cannot block SIGTERM past
systemd's timeout. Failure modes: flush-on-shutdown fails → coded error, never a silent
clean exit. Test plan: unit tests for the cap derivation and the 804 classification; a
source-order pin that the shutdown path signals the lane; bite-proofs for each guard.
Rollback: every item is independently revertable; 15/18 are constant changes, 17 is
additive. Observability: refusal counters already exist for oversize frames; the shutdown
flush gets an explicit outcome log.

## Items 15–21 Honest envelope

NOT claimed: that any of this has been exercised at 25,000 instruments or in Full mode —
neither has ever run. Item 15's 810 KiB figure is arithmetic from the documented packet
size and per-connection cap, not an observed frame. Item 16's ~70 GB/day is arithmetic
from row sizes, not a measurement. The measured evidence remains ~3,000 ticks/sec
aggregate in QUOTE mode; there is no Full-mode measurement in the repository at all.

---

## ITEMS 22–28 (added 2026-08-14) — operator: "yes dude fix evryhtign espeiclaly entilrey relaetd to this"

**The verbatim operator authorization (2026-08-14, typed directly in-session — preserve
EXACTLY, typos included):**

> "yes dude fix evryhtign espeiclaly entilrey relaetd to this dudde okay? see espeiclaly
> entiley related to linx kernel aws isnatcne see its betst toe ntolrey suie the entirte
> aws sinatcnec tunign eprformance optimisatiosn efficient to use ithe maixmsied entire
> ocnfirguatiosn dude ... i mena see not even a singke tikcs hsodu lenevr evr be lsot we
> hsodcu lalwya smaintian the dhan fast consumer espeicllay by mainitaning this becuase
> only then we can avoid websocket disocnenctiona nd websocket reconenciton espeicllay we
> hsodu lnto even face the smaller level of millsieocd latencye form dhan to aws"

Given in DIRECT response to a nine-agent audit whose fix queue named exactly these items.
**No new scope:** no socket beyond the authorized 16, no universe widening, `dry_run`
untouched, §28 frozen area untouched. Every item repairs code already landed.

### Design

**Item 22 — the required CI gate is RED (blocks everything else).** Commit `09fd3c2`
(today) introduced an unused binding at `operator_control.rs:675`. `Build & Verify` runs
`cargo clippy --workspace --no-deps -- -D warnings`, so this is a hard error — reproduced
locally at **exit 101**. Nothing else in this plan can merge until it is fixed. Rename the
binding to `_le`; the loop genuinely does not use it (the `+Inf` bound is read from
`bound`, and the comment below the loop explains why the string form is deliberately
unused).

**Item 23 — Nagle is enabled on all 16 sockets.** `connection.rs:762` calls
`connect_async_tls_with_config(request, config, false, Some(connector))`. The third
parameter is `disable_nagle`, so `false` leaves Nagle ON. The lane is receive-heavy, but
the client does send: subscribe batches (up to 250 messages) and — critically — **pongs**.
Dhan closes a socket after 40 s of silence (`99-tickvault-net.conf:123-124`), and Nagle
can hold a small write behind an unacknowledged segment for up to ~200 ms while delayed-ACK
waits on the peer. That is latency the operator explicitly asked to remove, on the one
write whose lateness costs a reconnect. Pass `true`.

**Item 24 — the TLS connector is rebuilt on every dial.** `connection.rs:718-722` calls
`build_websocket_tls_connector*()` inside the dial path, so each of the 16 sockets — and
every reconnect thereafter — constructs a fresh `rustls::ClientConfig`. Two costs follow:
(a) `rustls_native_certs::load_native_certs()` re-reads and re-parses the entire system CA
bundle **from disk** on the recovery path, which is exactly when the box is least idle;
(b) a fresh `ClientConfig` carries a fresh, empty session-resumption store, so every
reconnect is a full 2-RTT handshake instead of an abbreviated one. Cache both connector
variants in `OnceLock`s and hand out clones (`Connector::Rustls(Arc<ClientConfig>)` is
already `Arc`-backed, so a clone is a refcount bump). Resumption then works because all
sockets of a kind share one config.

**Item 25 — a heap allocation on every tick.** `record_ws_lag`
(`dhan_feed_stack.rs:1994,1998`) calls `connection_index.to_string()` and passes it as a
metric label. `metrics::histogram!` with a label builds a `Key` owning a `Vec<Label>`, so
this is **two heap allocations per tick** on the live path — the exact cost
`parser/dispatcher.rs:32-35` and `DrainCounters` (`:1158-1170`) both exist to avoid. The
label set is bounded and known at compile time (`MAX_TOTAL_DHAN_CONNECTIONS = 16`), so
resolve all 16 histogram handles plus the two excluded-counter handles once into a
`OnceLock<WsLagHandles>` and index by connection. This is the same fix, in the same file,
as the pattern two hundred lines above it.

**Item 26 — the silence pager has no trading-day gate.** `is_within_market_hours_ist`
(`dhan_feed_stack.rs:2964`) checks time-of-day only; the file imports `trading_calendar`
solely for `ist_offset()`. EventBridge starts the box on `MON-FRI`, which includes NSE
holidays. On such a day the lane dials, seeds ~4,565 instruments, receives nothing, and at
09:15 + two 30 s scans fires `RISK-GAP-03` with `silent=4565, never_ticked=4565` — the
most alarming page the system can produce, false, roughly six times a year. The second-order
cost is what matters: it trains the operator to mute the ONE detector that catches a
silently-failed subscribe. Every sibling leg already gates on `is_trading_day`
(`groww_contract_1m_boot.rs:1900`, `groww_option_chain_1m_boot.rs:1819`,
`brutex_crossverify_boot.rs:1673`, `feed_scoreboard_boot.rs:188`). Gate the page — not the
gauges, which are correct to publish zero on a holiday.

**Item 27 — the 15:31 cross-verification also fires on holidays.** Same root cause
(`dhan_feed_stack.rs:2828`), same fix. Today it warns "found no data on either side" every
weekday holiday, compounding Item 26's fatigue on the lane's only loss detector.

**Item 28 — two host-tuning gaps the kernel audit found.** The sysctl set is otherwise
strong (20 knobs, applied at boot and verified). Missing: (a) **VM dirty ratios** — QuestDB
shares this box, and an unbounded writeback burst can stall the host, which stalls the
consumer, which is the actual tick-loss trigger; set `dirty_ratio=10` /
`dirty_background_ratio=3` so writeback is frequent and small rather than rare and huge.
(b) **Transparent hugepages** left at the distro default; set `madvise` — not `never`,
because the co-tenant database benefits from THP and `never` would hurt it. Also assert
**chrony** in provisioning: AL2023 ships it pointed at the Amazon Time Sync service by
default, but nothing in `user-data.sh.tftpl` verifies it, and every latency number in Item
12 is meaningless if the clock is unsynchronised.

### Edge Cases

- `disable_nagle=true` must not alter the depth-200 no-ALPN path — the two are orthogonal
  (one is a socket option, one is a TLS config), and the depth-200 ALPN carve-out
  (`connection.rs:712-717`) must survive untouched.
- Cached TLS connector must NOT be shared between the ALPN and no-ALPN variants — they are
  different configs and crossing them re-opens the 2026-04-23
  `ResetWithoutClosingHandshake` class. Two separate `OnceLock`s, never one.
- A TLS build failure must still be reported per-dial (it currently increments
  `tls_config`); caching must not turn a transient first failure into a permanent poisoned
  `OnceLock`. Cache only on success.
- Lag handles: `connection_index` is a `u8` and must be bounds-checked against
  `MAX_TOTAL_DHAN_CONNECTIONS` before indexing — an out-of-range slot must fall back to a
  counted "unknown" bucket, never panic on the hot path and never allocate.
- Holiday gate must NOT suppress the gauges — a holiday reading zero silent instruments is
  correct data. Only the PAGE is gated.
- Holiday gate must not suppress a page on a trading day that merely starts late.
- `vm.dirty_ratio` must stay well above the writeback the ILP path can produce in one
  flush, or the flush itself starts throttling — 10% of 32 GiB is ~3.2 GiB, far above it.
- THP `madvise` is a boot-time write to `/sys`, not a sysctl — it must be applied in
  user-data and be idempotent across reboots and re-provisions.

### Failure Modes

| Failure | Detection | Response |
|---|---|---|
| Clippy regression reintroduced | `Build & Verify` (required check) | merge blocked |
| Nagle silently re-enabled by a future edit | ratchet asserting the literal `true` at the dial site | build fails |
| TLS connectors crossed (ALPN into depth-200) | ratchet asserting two distinct cached handles | build fails |
| Per-tick allocation reintroduced | pre-resolved handles + the Item 1 DHAT gate once it lands | build fails / gate catches |
| Holiday page fires anyway | unit test drives a known NSE holiday through the gate | build fails |
| sysctl file rejected as a whole by a bad key | `verify-net-tuning.sh` already checks 8 keys post-apply | boot marker absent, loud |
| chrony absent | new provisioning assertion logs loudly to journald | visible at boot |

### Test Plan

- `connection.rs` — unit test asserting the dial passes `disable_nagle = true`; source-order
  ratchet so the literal cannot silently flip back.
- `tls.rs` — test that two calls to each builder return handles sharing one `Arc`
  (`Arc::ptr_eq`), and that the ALPN and no-ALPN caches are NOT the same handle.
- `dhan_feed_stack.rs` — unit tests: all 16 connection slots resolve a distinct handle; an
  out-of-range index falls back and counts rather than panicking; existing `ws_lag_ms`
  behaviour (clamped-negative, implausible-LTT) is unchanged.
- Holiday gate — unit tests over a known NSE holiday from `base.toml`, a normal trading day,
  and a weekend, asserting page-suppressed / page-allowed / page-suppressed respectively,
  and asserting the gauge publishes in all three.
- Scope per `testing-scope.md`: `cargo test -p tickvault-core -p tickvault-app`; no
  `crates/common` change, so no workspace escalation.
- `cargo clippy --workspace --no-deps -- -D warnings` must return 0 — that is Item 22's
  acceptance test and it is reproduced before and after.

### Rollback

Every item is independently revertable and none changes a schema, a DEDUP key, a config
default, or a metric name. Item 22 is a rename. Item 23 is one boolean. Item 24 is a cache
in front of an unchanged builder — reverting restores per-dial construction. Item 25 is a
mechanical metrics refactor emitting the identical series, so dashboards and alarms cannot
notice a revert. Items 26–27 gate a page and are revertable by deleting the gate. Item 28
is host configuration applied at boot; reverting the sysctl file and re-running
`sysctl --system` restores prior behaviour without touching the app.

### Observability

No new metric series. Item 25 emits exactly the series it emits today
(`tv_dhan_ws_lag_ms{connection}`, `tv_dhan_ws_lag_excluded_total{reason}`), which is what
makes it a safe refactor, plus one bounded fallback label for an out-of-range slot. Items
26–27 REDUCE noise by suppressing false pages; the underlying gauges are unchanged, so a
real silence on a real trading day still pages exactly as it does now. Item 28 is visible
through the existing boot verification marker.

### Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: after these items the
CI gate is green, no write on the socket path waits on Nagle, reconnects reuse a TLS
config instead of re-reading the CA store from disk, the tick path allocates nothing to
record its own latency, and the lane's loudest page cannot fire on a day the market is shut.

**NOT claimed:** (a) that any of this improves Dhan's delivery — measured 2026-07-06 at
p99 46.37 s / max 198.69 s against a second vendor's 562 ms on the same host in the same
minutes; every item here is on our side of the NIC and moves that number by exactly zero.
(b) That millisecond-level end-to-end latency is measurable at all — Dhan's LTT is whole
seconds, so a ±1 s truncation floor is structural. (c) That the WAL re-fold (Item 2) is
addressed — it is NOT; replayed live-feed frames are still counted and dropped at
`main.rs:1841-1873`, and that remains the single largest known tick-loss path we control.
It is deliberately left to Item 2 because the fold path takes a live ring rather than a
replay batch, and a half-done version risks the 5 working main-feed sockets. (d) Any
measurement at 4,565 instruments — the lane has still never been observed receiving a tick.

## ITEM 2 — DESIGN ADDENDUM (added 2026-08-18, operator: "fix ebrythgine ntirley ddue okay?")

Item 2 above states the DEFECT and names the remedy ("re-fold from the WAL,
DEDUP-idempotent via `capture_seq`"). This addendum is the DESIGN, written before any
code, because the one-line remedy as stated is **not safe as written** — see §A2.

**Status of the defect today (re-verified 2026-08-18, in source):** the loss is REAL but
it is **not silent**. `WalRingSink::accept` returns `RingFull`; the frame is already in
the WAL; `pool_supervisor.rs` then emits a coded `error!` per socket, throttled at
1/2/4/8… occurrences, stating verbatim *"nothing re-folds WAL frames, so treat this as
data loss until that changes."* It is counted (`tv_dhan_ws_ring_full_total`) and charted.
What is missing is only the RECOVERY. Recording this so nobody re-reports "silent tick
loss" — that half was fixed 2026-08-11.

### A1. Why the boot path cannot simply be called mid-session

`refold_wal_frames` has exactly ONE production call site, and its safety argument is
positional, not defensive. The comment at that site states it is placed "AFTER seeding and
BEFORE any socket opens" so a recovered frame "cannot race a live frame for the same
`capture_seq`". Mid-session **every** premise of that sentence is false: sockets are live,
seeding is done, and live frames are arriving concurrently. Calling it from the drain loop
inherits none of its safety.

### A2. The correctness trap — re-folding a SUPERSET is NOT idempotent

Item 2's phrase "DEDUP-idempotent via `capture_seq`" is true of **ticks** and false of
**candles**, and the difference decides the whole design:

| Sink | Re-folding the same frame twice | Why |
|---|---|---|
| `ticks` table | **Safe** — collapses | DEDUP UPSERT KEYS `(ts, security_id, segment, capture_seq, feed)` include `capture_seq` |
| Candle aggregator | **CORRUPTS** | The fold ACCUMULATES: `volume` and `tick_count` are summed, so a re-folded tick double-counts them |

Therefore the recovery must re-fold **exactly** the refused frames — never "replay the
segment and let dedup sort it out." That approach would silently inflate volume on every
recovered candle, which is worse than the loss it repairs (wrong data beats missing data
only when it is labelled wrong; here it would be neither).

### A3. Design

1. **Record the refusal, not the recovery.** `WalRingSink::accept` already knows the exact
   moment a frame is refused. On `RingFull` it pushes that frame's `capture_seq` into a
   fixed-capacity `RefusedSeqs` tracker (no allocation; a pre-sized ring of `u64`).
2. **Bounded, fail-LOUD tracker.** Capacity is fixed at construction. If the tracker itself
   overflows, we have lost the identity of the lost frames — that is unrecoverable and is
   reported as such (`tv_dhan_refold_untracked_total` + a coded `error!`), never silently
   truncated into a partial recovery that reads like a full one.
3. **Recovery runs on the drain's own task, in its own bounded arm** — the established
   house shape (`scan_silence` has a dedicated 30 s timer precisely because it is O(n);
   `catch_up_seal_all` runs on the drain's `select!`). It must NOT run inside the flush
   path it is recovering from, and it must NOT run while the channel is still backed up:
   the trigger is "channel depth below a low-water mark AND `RefusedSeqs` non-empty."
4. **Exactly-once fold.** Each recovered `capture_seq` is removed from the tracker only
   after its fold returns, so a crash mid-recovery leaves it pending for the boot path.
5. **Late arrival is an EXISTING solved problem, not a new one.** A recovered tick whose
   bucket already sealed is a late tick, and the aggregator already has a late-tick policy
   (`FeedStrategy` / `LatePolicy`, Dhan = Refold). The recovery reuses it rather than
   inventing a second late path.

### A4. Edge Cases

| # | Case | Handling |
|---|---|---|
| 1 | Tracker overflows during a long stall | Unrecoverable-by-identity: count + coded error, never a partial recovery reported as whole |
| 2 | WAL segment rotated/archived before recovery | Frame unreadable: count + coded error; recovery skips it and continues |
| 3 | A second stall begins during recovery | Recovery is bounded per pass and re-armed; it never competes with live folding |
| 4 | Socket reconnects mid-recovery | `capture_seq` is replay-stable, so identity survives the reconnect |
| 5 | Recovered tick's bucket already sealed | Existing late-tick policy applies (§A3.5) |
| 6 | Frame was refused but ALSO folded (double-path) | Impossible by construction: `accept` returns exactly one outcome per frame |
| 7 | Recovery finds an empty tracker | No-op, no log, no cost |

### A5. Failure Modes

| Mode | Consequence if unhandled | Mitigation |
|---|---|---|
| Re-fold a superset | Inflated candle volume/tick_count — **wrong data** | §A2: exact-seq recovery only |
| Recovery starves live folding | Live lag grows while repairing history | Bounded work per pass + low-water trigger |
| Unbounded tracker | Memory growth under sustained stall | Fixed capacity, fail-loud on overflow |
| Silent partial recovery | Loss reported as repaired | Overflow and unreadable-frame counters are separate and coded |

### A6. Test Plan

1. `refused_seq_is_recorded_on_ring_full` — accept returns `RingFull` ⇒ seq present.
2. **`refold_twice_does_not_double_count_volume`** — the crux of §A2. Fold a tick, re-fold
   the same frame, assert candle `volume`/`tick_count` unchanged. Expected to FAIL against
   a superset-replay implementation; that is the point of writing it first.
3. `tracker_overflow_is_counted_and_never_silently_truncated`.
4. `recovery_is_bounded_per_pass` — N refused frames ⇒ ≤ budget folds per pass.
5. `recovery_does_not_run_while_channel_is_backed_up`.
6. `unreadable_wal_frame_is_counted_not_panicked`.
7. DHAT: recovery path allocates zero per recovered frame in steady state.

### A7. Rollback

Recovery ships behind a config gate, serde default **OFF**. Flipping it off restores
today's exact behaviour (loss + loud log). No schema change, no DEDUP-key change, no new
table — so rollback is a config flip and a restart, never a migration.

### A8. Observability

New: `tv_dhan_refold_recovered_total`, `tv_dhan_refold_untracked_total`,
`tv_dhan_refold_unreadable_total`. **Mandatory in the same PR:** the existing `RingFull`
error text states *"nothing re-folds WAL frames"* — that sentence becomes FALSE the moment
this lands, and a stale operator-facing message is the false-OK class the charter forbids.
It must be updated in lockstep.

### A9. Honest envelope

100% inside the tested envelope: frames REFUSED by a full ring are recovered exactly-once
from the durable WAL, bounded per pass, with overflow and unreadable frames counted
separately and loudly. **NOT claimed:** recovery of frames the WAL never received (a frame
lost before `accept` is not in scope and never was); recovery after WAL rotation (§A4.2 —
counted, not repaired); that recovery is free (it competes for the drain's task and is
therefore rate-limited by design); or any live verification — this design is UNVERIFIED
against a real stall, and the first live stall is the probe.

### A10. BLOCKER found while attempting implementation (2026-08-18) — the WAL has no bounded reader

Implementation was attempted immediately after §A1–A9 and **stopped at a defect in
this design's own §A3.4**, recorded here rather than worked around.

§A3 says "re-fold from the durable WAL". The WAL's ONLY read API is
`ws_frame_spill::replay_all(wal_dir) -> anyhow::Result<Vec<ReplayedFrame>>`, and it is
**boot-shaped, not session-shaped**:

| Property | Value | Consequence mid-session |
|---|---|---|
| Segment selection | globs **every** live `*.wal` plus `replaying/` leftovers | not "the stalled window" — the whole day so far |
| Return type | `Vec<ReplayedFrame>`, each owning `frame: Vec<u8>` | every frame copied onto the heap at once |
| When segments leave the glob | only after `confirm_replayed` moves them to `archive/` | nothing has left yet during a live session |

At the documented envelope (~5,000 packets/sec, frames up to 256 KiB on the main feed)
a mid-session `replay_all` would load the **entire session's captured frames** into
memory in one allocation burst, on the drain task, on a 32 GiB box that is also running
QuestDB. **That is an OOM, not a recovery** — and it would fire precisely during a
database stall, i.e. exactly when the system is already degraded. Calling it would also
be wrong in a second way: `confirm_replayed` must NOT run mid-session or it would
archive segments the boot path still needs.

**Therefore Item 2 gains a hard prerequisite** — a bounded, streaming WAL read:

- read frames matching a supplied `capture_seq` set WITHOUT materialising the segment
  (iterator/callback, not `Vec`);
- bounded per call (a cap on frames returned per recovery pass);
- tolerant of the ACTIVE segment being appended to concurrently — a torn final record
  must be skipped, never parsed (the boot path only ever reads quiescent files, so this
  requirement is genuinely new, not inherited);
- must NOT mutate segment state (no `confirm_replayed`, no move to `replaying/`).

`ReplayedFrame` already carries `frame_seq`, so the identity needed for exactly-once
filtering exists — what is missing is a way to reach it without loading everything.

**Honest consequence for sequencing:** Item 2 is NOT a single-crate change and cannot
land as one PR. Order is (1) the bounded reader in `crates/storage`, with its own tests
including the torn-tail case, then (2) the refused-seq tracker, then (3) the drain's
recovery arm. Attempting (2) or (3) first produces code with nothing safe to call.

**Status:** design AMENDED, implementation **NOT started** — deliberately. Shipping the
naive version would have converted a bounded, loud, counted tick loss into an
out-of-memory kill of the whole lane during a database stall. That trade is strictly
worse than the defect it repairs.

## ITEM 22 (added 2026-08-19) — the 25,000-instrument contract universe

**Operator:** *"You motherucker I clelarug told yo uto Goa head with 25k
sinturments with dperh 5 with full mode right motherfucker see is this clealry
done built an dwored or not motherucker okay? Menabwile what about depth 20 of
250 instruments dude and what about depth 200 of 5 instruments and order update
entie capturing dude okay? I need all these now to be build wired integrated
implemented dude okay?"* (2026-08-19)

Authority for the SET is the 2026-08-15 "FULL-MODE, FULL-UNIVERSE SUBSCRIPTION
SCOPE" section of `websocket-connection-scope-lock.md`. This item is the
implementation of that authorization, which had been recorded and never built.

### The finding that made it necessary

Verified in source before any code was written: the live universe resolves
through `master_csv::is_nse_cash_equity` — `NSE && E && EQ`. Cash equity only.
The authorized ~24,600 set is ~90% option contracts, and a cash-equity filter
can never emit one. The lane was subscribing ~4,565 spots in Full mode with
depth on all of them, which is real and is not what was authorized.

Second finding, same audit: depth-20 was carrying **84 of its 250 slots**. Its
fixed ±10 window was justified in its own doc as "126 instruments across 3
underlyings", but Dhan serves depth on NSE only, so SENSEX (BSE_FNO) is refused
every day and the eligible set is two underlyings, not three.

### Plan Items

- [x] Parse the five derivative columns from the Dhan master (INSTRUMENT,
      SM_EXPIRY_DATE, STRIKE_PRICE, OPTION_TYPE, UNDERLYING_SYMBOL), typed and
      compact, all OPTIONAL so a six-column header still parses
  - Files: crates/core/src/instrument/master_csv.rs
  - Tests: test_a_six_column_master_still_parses_and_reads_as_no_derivatives,
    test_a_detailed_master_row_carries_its_derivative_fields,
    test_expiry_accepts_both_vendor_forms_and_refuses_the_rest,
    test_expiry_packing_orders_the_same_way_calendar_time_does,
    test_strike_rounds_to_paise_and_refuses_nonsense,
    test_unknown_instrument_and_option_codes_classify_rather_than_panic,
    test_class_predicates_split_options_from_futures,
    test_a_row_too_short_for_an_optional_column_is_kept_not_rejected
- [x] Contract selection: futures all expiries, index chains current expiry,
      stock options ATM ± 25, capacity-bounded by shrinking the ATM window
  - Files: crates/app/src/dhan_contract_universe.rs, crates/app/src/lib.rs
  - Tests: a_full_shape_selection_reaches_the_authorized_scale,
    the_atm_window_shrinks_rather_than_the_selection_truncating,
    an_underlying_with_no_spot_price_is_refused_not_guessed,
    every_underlying_gets_the_same_window_regardless_of_iteration_order,
    futures_and_index_chains_outrank_stock_options_under_pressure,
    the_same_id_in_two_segments_is_kept_as_two_instruments,
    the_selection_is_deterministic_across_runs
- [x] Adaptive depth-20 window sized to the ELIGIBLE underlying count, filling
      244 of 250 instead of 84
  - Files: crates/app/src/dhan_depth_universe.rs
  - Tests: the_depth_20_window_fills_the_envelope_without_exceeding_it,
    the_two_eligible_underlyings_case_fills_the_pool,
    no_eligible_underlyings_selects_no_window_rather_than_dividing_by_zero,
    a_very_wide_underlying_set_degrades_to_the_money_rather_than_overflowing,
    the_pool_envelope_is_derived_from_the_vendor_limits_not_written_down
- [x] Contract artifact written by the daily rider, read by the attach
  - Files: crates/app/src/dhan_universe.rs, crates/app/src/dhan_contract_universe.rs
  - Tests: the_artifact_round_trips_to_the_same_selection_as_a_parsed_master,
    a_contract_row_survives_serialisation,
    the_artifact_path_is_dated_so_a_stale_day_is_never_read
- [x] Live spot prices from QuestDB, joined to underlyings via the symbol map
  - Files: crates/app/src/dhan_contract_universe.rs
  - Tests: the_spot_query_bounds_to_today_and_keys_on_the_composite,
    spot_prices_parse_to_paise_and_refuse_the_unusable,
    the_symbol_map_reads_names_and_survives_a_bad_row,
    the_join_prices_only_symbols_that_actually_ticked,
    the_join_will_not_price_a_stock_off_its_own_derivative
- [x] Contract dial on the existing late-attach retry loop, bounded by the
      connections the spot universe already consumed
  - Files: crates/app/src/dhan_feed_stack.rs
  - Tests: remaining_capacity_is_counted_in_whole_connections,
    a_full_pool_leaves_no_room_rather_than_overflowing_it,
    connection_count_rounds_up_because_a_partial_socket_is_still_a_socket,
    the_two_helpers_can_never_together_exceed_the_authorized_pool,
    a_malformed_date_selects_nothing_rather_than_an_expired_contract,
    the_date_packing_matches_the_one_expiries_are_stored_in

## Item 22 Design

Two resolution times, because they need different evidence. The daily rider
(08:30) has the master and writes the derivative subset as an artifact. The
attach (post-open) has live prices and does the selection, because locating
at-the-money needs a price that does not exist at boot.

Contracts ride the EXISTING depth late-attach retry loop rather than a second
task. Not for tidiness: both wait on post-open evidence, and both dial through
a `PoolSupervisor` that cannot be owned by two tasks at once.

Priority order under capacity pressure: index futures, stock futures, index
chains, then stock options. The ATM window is the only elastic dimension, and
it shrinks uniformly across underlyings — chosen before anything is pushed, so
an early stock cannot take 25 strikes while a late one takes three.

## Item 22 Edge Cases

- Six-column master (every existing fixture): parses, derivative fields read
  as absent, equity join unchanged.
- Underlying with no tick today: options REFUSED and counted, never centred on
  a guessed at-the-money.
- Ladder shorter than the window: taken whole, no index panic on vendor data.
- Same numeric id in two segments: kept as two instruments (I-P1-11).
- Malformed IST date or expiry: packs to 0, which compares as "before today",
  so nothing is selected rather than an expired contract being subscribed.
- Spot universe already using all 5 main-feed connections: contracts get 0
  capacity rather than a sixth connection.
- Envelope smaller than the futures alone: truncated AND counted.

## Item 22 Failure Modes

- Contract artifact unwritable: coded error, spot universe unaffected, the
  session carries no contracts and says so.
- Artifact unreadable at attach: empty selection, coded error naming the path.
- Symbol map unreadable: futures and index chains still selected; stock
  options refused for want of prices.
- QuestDB spot query fails: empty price map, stock options refused, futures
  and index chains unaffected.
- Every failure returns an EMPTY selection, never a partial one presented as
  complete.

## Item 22 Test Plan

`cargo test -p tickvault-core --lib master_csv` (34) and
`cargo test -p tickvault-app` (1,448 lib + 82 binaries). Pure functions
throughout the selection path, so every hostile case is a fixture rather than
a mock: no I/O, no clock, no network in `select_contract_universe`.

## Item 22 Rollback

Set `[dhan_universe] live_subscription_from_master = false` to restore the
4-SID index universe. The contract attach then finds an artifact it does not
need and selects nothing extra. No schema change, no migration; the artifact is
a dated file that is simply not read.

## Item 22 Observability

`contract universe resolved` logs artifact size, priced underlyings, selected
count, per-class counts, the ATM window actually used, underlyings without a
spot, and contracts dropped for capacity. The late-attach success line reports
the same alongside the depth counts. Every refusal in `ContractSelection` is a
counted field, so a short selection is never mistaken for a thin market.

## Item 22 Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: selection
is a pure function proven to reach 22,660 instruments on a 220-stock shape, to
stay inside the envelope at every underlying count from 1 to 8, and to refuse
rather than guess when a price is missing.

NOT claimed: that any of this has received a tick. The Dhan live lane has not
reported a non-zero cross-verification `compared` count since the 2026-07-13
retirement, so contract selection is code that is ready for a feed that is
still unproven. NOT claimed: CPU at 25,000 instruments — the drain has never
run above ~4,565 and the ~12,500 packet/sec figure is arithmetic, not a
measurement. NOT claimed: index FUTURES depth, which no authorized Dhan source
can reach. NOT claimed: that the ~24,600 figure will be met on any given day —
it is whatever the master resolves, bounded by the connections left after spot.

## ITEM 23 (added 2026-08-21) — operator: "see evrythgin enitlrey it shdou lbe always full mode with depth 5" / "yes fix evrythgin"

**Scope authority:** the dated 2026-08-21 section in
`websocket-connection-scope-lock.md` ("FULL MODE EVERYWHERE, and the ONE segment
that cannot take it"), recorded BEFORE this code change per the rule-file-first
law. No new scope: no socket opened, no universe widened, `dry_run` untouched,
§28 frozen area untouched.

## Item 23 Design

The 119 subscribed NSE indices have produced zero ticks since being subscribed,
while 8,868 tradeable instruments flow normally. Root cause: the rebuilt lane
applies ONE global feed mode to every instrument. Since 2026-08-19 that mode is
Full (21), which requests 5 levels of order-book depth. An index has no order
book, and Dhan answers the request with silence rather than an error.

Fix: restore the per-segment split the pre-retirement lane had
(`idx_instruments`, lost in the 2026-07-17 delete / 2026-08-09 rebuild).

- `IDX_I_FEED_MODE: FeedMode = FeedMode::Quote` — one constant, one line to
  change if the live probe says Ticker instead.
- `feed_mode_for_segment(segment, configured)` — pure, total, O(1). Indices
  take the override; every other segment returns `configured` UNCHANGED, so a
  future scope-lock mode move still propagates.
- `partition_index_batch(batch)` — pure, cold-path, zero-loss.
- `send_subscribe` splits main-feed batches and sends one message per mode.
  One Dhan message carries exactly one `RequestCode`, so this is structural.
  Depth endpoints are untouched (they refuse non-NSE segments and never carry
  an index).

Files: `crates/core/src/websocket/connection.rs`.

## Item 23 Edge Cases

| Case | Behaviour |
|---|---|
| Batch with no index | Original path, byte-identical to before |
| All-index batch | One Quote message; no empty second message (`EmptyBatch` would tear the socket down) |
| Mixed batch | Two messages: others in configured mode, indices in Quote |
| Non-main-feed endpoint | Never splits |
| Batch ≤100 mixed | Each half is ≤100, so neither can trip `BatchSplit` |

## Item 23 Failure Modes

| Failure | Result |
|---|---|
| The non-index half fails to send | `?` propagates, socket torn down — indices are NOT sent against a half-subscribed socket |
| The index half fails to send | Returned as failure; the supervisor re-subscribes |
| Quote also refused by Dhan for IDX_I | Identical symptom (silence), caught by the unchanged RISK-GAP-03 never-ticked count — the stated verification |
| Override wrongly applied to a real segment | Would drop ~24,600 instruments off depth-5 — pinned against by `every_other_segment_keeps_the_configured_full_mode` |

## Item 23 Test Plan

Seven tests in `connection.rs`, all green (51 in module):
`feed_mode_for_segment_gives_an_index_quote_not_full`,
`feed_mode_for_segment_keeps_full_for_every_other_segment` (non-vacuity),
`feed_mode_for_segment_passes_the_configured_mode_through`,
`feed_mode_for_segment_makes_a_mixed_batch_two_messages`,
`partition_index_batch_loses_nothing_and_duplicates_nothing` (zero-loss),
`partition_index_batch_puts_an_all_index_batch_on_one_side`,
`partition_index_batch_with_no_index_leaves_the_original_path`.

**Bite-proven:** reverting `IDX_I_FEED_MODE` to `Full` fails
`feed_mode_for_segment_gives_an_index_quote_not_full` and
`feed_mode_for_segment_makes_a_mixed_batch_two_messages` (verified 2026-08-21).

Also CORRECTS `test_the_main_feed_payload_carries_the_full_request_code`, whose
fixture was an `IDX_I` instrument asserting it gets code 21 — the defect written
down as expected behaviour, the same shape as the assertion corrected in
`a2acb6a`. Fixture is now a tradeable instrument.

## Item 23 Rollback

Single constant: set `IDX_I_FEED_MODE = FeedMode::Full` to restore today's
behaviour exactly (the partition then sends two messages carrying the same
code, which is harmless). Full revert is one file.

## Item 23 Observability

No new metric or alarm. The existing `RISK-GAP-03` never-ticked counter is the
verification and is already alarmed: it has read exactly the IDX_I count every
day (4, then 119) and must read 0 for the index set after this lands.

## Item 23 Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: the mode
selection is a pure total function with non-vacuity and pass-through pins, and
the partition has a zero-loss property test. **NOT claimed: that indices now
tick.** Whether Dhan SERVES Quote for IDX_I on this account is UNVERIFIED-LIVE —
the May 2026 support ticket asking exactly that has no recorded answer, and an
older uncited note claimed Ticker is forced. If Quote is also refused the
symptom is identical (silence) and `IDX_I_FEED_MODE` is the one line to change.
What IS claimed: the lane no longer sends indices a request Dhan is measured to
answer with nothing, and the failure remains loud rather than silent.

## ITEM 24 (added 2026-08-21) — operator: "fix everythgin dude okay?" — live ticks had no rescue tier

**Scope authority:** no scope change — no socket, no universe, no `dry_run`,
no §28 edit, no new Telegram page (so no noise-lock row is required). This
repairs a loss path inside code that already shipped.

## Item 24 Design

**Measured today:** two flush failures discarded **1,377 unrepeatable ticks**.
Root cause, from the live error chain:

```
Caused by: Could not flush buffer: http://localhost:9000/write: timeout: per call
```

A CLIENT-side HTTP timeout. QuestDB's own container log has no error at either
timestamp, so nothing was rejected and the buffer was never poisoned. Three
different writers (ticks, `spot_1m_rest`, `option_chain_1m`) hit it in the same
window, which is the signature of shared write-latency pressure rather than a
bad row. The disk is NOT the cause — the Quote-17 EBS raise is already applied
live (200 GB / 6000 IOPS / 500 MiB/s, verified via `describe-volumes`).

`TickWriter::discard_pending` was clearing the whole buffer on ANY flush error.
The "poisoned-buffer defence" rationale is real but applies to a row the SERVER
refused; it was being applied to every failure alike. `seal_absorption` has had
a ring → spill → DLQ chain for seals since the beginning; the ticks writer —
carrying the one payload class that CANNOT be re-fetched — had no rescue tier
at all. The REST writers' own message says "rows are re-fetchable", which is
true for them and is exactly why they keep the plain discard.

Fix: rescue to a spill tier instead of dropping.

- `spill_failed_ilp(dir, payload, feed, now)` appends `Buffer::as_bytes()`
  **verbatim**. That payload is InfluxDB line protocol — the exact body
  QuestDB's `/write` accepts — so recovery is one command, not a bespoke
  parser: `curl --data-binary @<file> http://<questdb>:9000/write`. Replay is
  idempotent because the `ticks` DEDUP key carries `capture_seq`.
- One file per feed per hour: bounded file count, and a known-bad window can be
  replayed without re-ingesting the day.
- `TICK_SPILL_MAX_BYTES` (512 MiB) is a real ceiling. QuestDB and the frame WAL
  share this volume, so an unbounded rescue would trade a bounded tick loss for
  an unbounded outage of everything. Past the cap the rows drop and are counted
  — the same honest failure as today, never a worse one.
- `spill_dir` is a FIELD, not a constant, so the rescue path itself is testable.

Files: `crates/storage/src/tick_persistence.rs`.

## Item 24 Edge Cases

| Case | Behaviour |
|---|---|
| Empty buffer | No file written (non-vacuity test) |
| Two failures, same feed, same hour | Appended to one file; never truncated |
| Different hour / different feed | Separate file |
| Spill dir missing | Created |
| Cap reached, disk full, no permission | Falls back to the counted drop, loudly |
| Successful flush | Rescue never runs |

## Item 24 Failure Modes

| Failure | Result |
|---|---|
| Rescue write fails | `tv_ticks_dropped_total` + `error!` naming the spill error — the loss is never masked |
| Cap reached during a long outage | Bounded: drops resume, counted, disk protected |
| Operator never replays a spill file | Rows are not in QuestDB. Stated plainly — this makes loss RECOVERABLE, not automatically recovered |

## Item 24 Test Plan

Seven tests, all green (31 in module, 46 storage suites, zero failures):
`spill_failed_ilp_writes_the_payload_verbatim_so_it_can_be_replayed` (the
replayability contract), `..._appends_rather_than_truncating_a_second_failure`,
`..._separates_feeds_and_hours`,
`spill_dir_bytes_sums_only_files_and_survives_an_unreadable_dir`,
`the_spill_cap_is_a_real_ceiling_not_a_suggestion`,
`discard_pending_on_an_empty_buffer_writes_no_spill_file` (non-vacuity), and
`discard_pending_rescues_real_rows_to_the_spill_instead_of_dropping_them` —
the last exercising the REAL path, not the helper.

**Bite-proven:** forcing the rescue to fail makes 5 of them fail, including the
end-to-end one.

## Item 24 Rollback

Delete the `spill_failed_ilp` call from `discard_pending`; behaviour returns to
the bare discard. One function, one file.

## Item 24 Observability

New counter `tv_ticks_spilled_total{feed}`, pre-registered at 0 in
`register_drop_baseline` for the same delta-baseline reason the drop counter is
— a rescue episode is rare, so its first increment would otherwise be consumed
as the baseline and go unreported.

**CORRECTED 2026-08-21, same day, by the failure audit.** The first version of
this item said the new counter being unalarmed was fine, "matching the
neighbouring counters". That justification was FALSE, and its wrongness is
exactly what concealed a regression: `tv_ticks_dropped_total` — the counter the
rescue diverts traffic AWAY from — IS EMF-selected and IS alarmed
(`dhan_ticks_dropped`, `live-lane-alarms.tf`). So the change was not "one more
unwatched counter"; it was a DIVERSION that took the common flush failure off
the only pager watching it. Net paging coverage went DOWN, and a reader of the
original note would have concluded nothing changed.

Fixed by incrementing BOTH on the rescue arm. The alarmed counter is also the
semantically correct one there: it means "rows left the buffer without reaching
QuestDB", which is true of a rescued row. `tv_ticks_spilled_total` is the
narrower "and it is recoverable". Pinned by
`the_rescue_path_still_increments_the_alarmed_counter`, a source-scan ratchet
chosen over a recorder assertion because what must never regress is the ALARMED
NAME, and a recorder test would still pass against a different unalarmed name.

## Item 24 Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: the spill
is byte-verbatim (pinned), append-not-truncate (pinned), capped (pinned),
fail-soft to the counted drop (pinned), and never fires on a healthy writer
(pinned) — all bite-proven. **NOT claimed: that no tick is lost.** This converts
a permanent in-memory discard into a replayable on-disk file; the rows are not
in QuestDB until someone runs the documented curl, and past the 512 MiB cap
they still drop. **NOT claimed: that the flush timeouts stop** — the cause is
QuestDB write latency under the live load and is untouched here; this bounds
the consequence, not the cause.

## ITEM 25 (added 2026-08-21) — operator: "go ahead with the recommendation" + "one and only RUST O(1) in the entire workspace"

**Scope authority:** no scope change — no socket, no universe, no `dry_run`, no
§28 edit, no new Telegram page. Three defects and two documentation corrections,
all found by a three-agent audit run in parallel.

## Item 25 Design

**A. The never-traded sentinel timestamp (`row_timestamp_ist_nanos`).**
Measured live: **959,671 rows/session** carried `exchange_timestamp = 315,532,800`
(1980-01-01, Dhan's never-traded sentinel) or a literal `0`, stamping them into
permanent `1980-01-01` / `1970-01-01` partitions where **no time-range query can
reach them**.

The first recommendation was to DROP these rows. **That was wrong and the
verification caught it:** 99.2% of them carry a real `total_buy_qty` (max
8,397,000), `total_sell_qty` (max 9,019,000) and previous `close` (max 73,186.8).
They are contracts with a live order book that have not traded yet — the drain
keeps them deliberately, and the existing code comment saying discarding them
"costs the packet's open interest and bid/ask" is correct.

So the row is KEPT and its designated timestamp is taken from the RECEIPT time,
which is the only real time such an observation has. The raw sentinel survives
in `exchange_timestamp`, so "never traded" stays recoverable. The plausibility
floor is SHARED with the aggregator (`MIN_PLAUSIBLE_EXCHANGE_TS_SECS`) so the
candle refusal and the timestamp fallback cannot drift apart. Convention-safe by
construction: both inputs are already IST-epoch, so the function only CHOOSES
between two values in one space and can never introduce the `+19800` sign error
that `data-integrity.md` calls the single most critical rule.

**B. The alarm regression Item 24 introduced (`discard_pending`).**
Item 24's spill rescue incremented ONLY `tv_ticks_spilled_total`, which is
neither EMF-selected nor alarmed, and thereby DIVERTED the common flush failure
off `dhan_ticks_dropped` — the only pager watching it. Net paging coverage went
DOWN. Item 24's own note called that acceptable "matching the neighbouring
counters"; that justification was FALSE and its wrongness is what concealed the
regression. Fixed by incrementing BOTH; that Observability section is corrected
in place.

**C. Two now-false operator-facing claims** in `dhan_feed_stack`: the flush
`error!` still said the rows were "a counted loss", "nothing re-inserts them"
and "boot replay DROPS live-feed frames (there is no re-fold path)". All three
are false — the rescue exists and `refold_wal_frames` exists — so two ERROR
lines fired for one event with OPPOSITE verdicts, telling the operator recovery
was impossible when two recoveries work.

**D. Rust-only.** The audit found **zero executing violations** workspace-wide.
Two mentions fixed: an operator runbook (ours, not vendor reference, so outside
every carve-out) carried two Python blocks instructing a human to run them —
re-expressed as `curl` with identical headers and cookie warm-up; and a launcher
comment claimed a parity harness "re-materializes" a deleted interpreted server,
which was retired 2026-08-01. One mention KEPT deliberately: a third-party PyPI
package named in a REJECTED-options table, where removing the name would make
the table unable to say what was rejected.

**E. CLAUDE.md honesty.** The codebase map's `pipeline/` row named a
tick-processor module that does not exist and said 21 timeframes where
`TF_COUNT` is 24. Four unbounded-growth paths were missing from the O(1) table.

Files: `crates/storage/src/tick_persistence.rs`,
`crates/app/src/dhan_feed_stack.rs`, `CLAUDE.md`,
`docs/operator/nse-trading-calendar-2026.md`,
`scripts/mcp-servers/tickvault-logs-launch.sh`.

## Item 25 Edge Cases

| Case | Behaviour |
|---|---|
| Plausible LTT | Exchange time kept — unchanged for every normal row |
| LTT exactly AT the floor | Plausible, kept (boundary pinned) |
| LTT one second below | Sentinel, falls back |
| Sentinel LTT **and** no receipt time | Sentinel kept — a guess is worse than a visibly wrong value |
| Spill succeeds | BOTH counters fire, so the alarm still pages |
| Spill fails | Drop counter only, as before |

## Item 25 Failure Modes

| Failure | Result |
|---|---|
| Fallback wrongly applied to a real tick | Every price stamped with receipt time instead of trade time — pinned against by the plausible-LTT test |
| Someone re-diverts the rescue off the alarmed counter | `the_rescue_path_still_increments_the_alarmed_counter` fails the build |
| Floor drifts from the aggregator's | Impossible — the constant is shared, and pinned at the boundary |

## Item 25 Test Plan

Seven new tests, 38 green in `tick_persistence` (46 storage suites, 65 common,
1,533 app lib). **Bite-proven twice:** removing the timestamp fallback fails 4;
the alarm ratchet is a source-scan chosen over a recorder assertion because what
must never regress is the ALARMED NAME, and a recorder test would still pass
against a different unalarmed name.

Also demonstrated: the first draft of the CLAUDE.md correction NAMED the missing
module and `claude_md_codebase_map_guard` REJECTED it — the guard bites on
corrective prose too, which is the right behaviour.

## Item 25 Rollback

Each part is independent: drop the `row_timestamp_ist_nanos` call to restore
sentinel stamping; drop one `metrics::counter!` to restore Item 24's behaviour;
the rest are documentation.

## Item 25 Observability

No new metric. The change RESTORES `dhan_ticks_dropped` paging on the rescue
path. Sentinel rows become visible to time-range queries for the first time —
which will raise apparent tick counts by ~3%; those are real observations, and
consumers wanting traded ticks only should filter `ltp > 0`.

## Item 25 Honest envelope

100% inside the tested envelope, with bite-proven ratcheted coverage. **NOT
claimed:** that the flush timeouts stop — their cause is QuestDB write latency,
untouched here, though removing ~950k out-of-order writes per session should
reduce the pressure and that is a hypothesis, not a measurement. **NOT claimed:**
that the spill is automatically replayed — it still needs one curl, which does
not meet the zero-manual-intervention bar and remains open. **NOT claimed:** that
the audit was exhaustive — it found this table incomplete for the fourth time,
which is itself evidence that a source scan finds what it is pointed at.

---

## ITEM 26 (added 2026-08-21) — operator: "Go ahead wirh your recommendation"

**Status:** VERIFIED

### Design

Close the silent-loss hole between `confirm_replayed` and the WAL re-fold.

`main.rs` skips its own drop-and-log for recovered live-feed frames whenever
`dhan_lane_will_refold` is true, then calls `confirm_replayed`, which MOVES the
staged segments into the archive so they can never be offered again. Only after
that does the Dhan lane spawn and re-fold. The gate reads config plus one env
var — it is the same gate the lane uses, so the two can never disagree about
ENABLEMENT, and that is exactly why the hole is invisible: it opens on RUNTIME
refusal, which the gate cannot see.

Six bring-up paths return before the re-fold: an unplannable pool, a
`[rest_candle_fold]` collision, missing cross-verify deps, a missing WAL, a
token manager that never registered, and a duplicate spawn. Each returns loudly
about ITSELF and says nothing about the frames it was handed. So captured ticks
disappeared with no line anywhere — the silent-loss class the whole
capture-at-receipt chain exists to prevent.

`report_unfolded_wal_frames(frames, refusal)` is called at all six. It emits the
same `WS-GAP-03`-coded error and increments the SAME
`tv_ws_frame_wal_reinjected_dropped_total{ws_type="live_feed"}` counter that
`main.rs` uses for its own drop path, so the two paths sum into one number
instead of two. The gate-disabled return in `spawn_dhan_feed_stack` deliberately
does NOT call it: `main.rs` reads the identical gate and has already logged
those frames, and reporting again would double-count them.

### Edge Cases

- **Empty batch** — the normal boot. Returns without logging; a loss line
  printed every morning is a loss line nobody reads.
- **Gate disabled** — covered by `main.rs`, deliberately not covered here
  (double-count).
- **Duplicate spawn** — production has one call site and `FEED_STACK_SPAWNED`
  makes a second impossible, so this is test-only today. Covered anyway,
  because "impossible today" is how the other five started.
- **A seventh refusal added later** — the ratchet fails the build.

### Failure Modes

What this does NOT fix, stated plainly: it buys VISIBILITY, not recovery. The
raw frames stay in the WAL archive and still need a manual replay, which is the
same open item ITEM 24 records. Moving the confirm after the fold would buy
recovery, but the lane is a spawned task the boot path cannot await, and not
confirming at all re-stages the segments every boot forever (the WS-REINJECT-01
growth-storm class). That trade is recorded at the confirm site rather than
silently taken.

### Test Plan

- `report_unfolded_wal_frames_is_silent_on_an_empty_batch`
- `report_unfolded_wal_frames_names_the_refusal_it_was_given` — pins the
  structured field AND that the counter matches `main.rs`'s
- `every_lane_refusal_before_the_refold_reports_its_unfolded_frames` — the
  ratchet. Scans `run_dhan_feed_stack` up to the re-fold call and fails if any
  `return;` is not immediately preceded by a report. Bite-proven: removing the
  `wal_missing` call turns it red and names the offending line. Carries a
  non-vacuity assertion so a collapsed scan window cannot pass silently.

Verification: 1,536 app lib tests ok; 42 integration guard binaries that scan
`dhan_feed_stack.rs` or `main.rs` all ok; `cargo fmt --check` clean; CI-equivalent
`cargo clippy --workspace --no-deps -- -D warnings` clean.

### Rollback

Revert the commit. The reporter is additive — removing it restores the previous
(silent) behaviour exactly; no data path changes.

### Observability

`tv_ws_frame_wal_reinjected_dropped_total{ws_type="live_feed"}` — existing
counter, now incremented from six previously-silent paths. **FLAGGED, not
fixed:** this counter has no CloudWatch alarm, so the loss is now *loggable and
countable* but still not *pageable*. It joins the ~8 metrics absent from the EMF
selectors that ITEM 25's audit recorded; alarming it is its own change with its
own cost line.

### Rider — a stated property that had become false

`alive_connection_guard_tests` carried a comment claiming its tests were
"serialized ... by running both assertions here". That was true when the module
held one test and became false the moment a second one was added beside it: both
mutate the process-global `ALIVE_CONNECTIONS` and assert EXACT values, so under
`cargo test` (one process, many threads) they race. Adding tests to the same
binary shifted the scheduling and exposed it — 2 failures that vanish under
`--test-threads=1`.

Fixed the way this repo already fixed the identical class in
`tv_api_token_prod_guard.rs` (recorded in `merge-gate-lock-2026-07-04.md` §3.2):
a module-scoped `Mutex` with poisoning recovered rather than propagated, since
one of these tests raises a panic deliberately. The false comment is replaced by
one that says why the lock exists. nextest gives each test its own process,
which is why CI never showed this.

### Per-Item Guarantee Matrix

Carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
(15-row + 7-row). Rows specific to this item:

- **Zero ticks lost** — no new drop path; this item makes an EXISTING silent
  drop audible. Honest: it does not recover the frames.
- **Logging** — every new path uses `error!` with
  `code = ErrorCode::WsGapConnectionState.code_str()`.
- **Extreme check** — the ratchet fails the build on a seventh unguarded return,
  bite-proven in both directions.
- **O(1)** — the reporter is O(1): a length read, one log, one counter
  increment. No allocation beyond the log record itself, and it runs only on a
  refusal path, never on the hot path.

---

## ITEM 27 (added 2026-08-21) — operator: "Go ahead wirh your recommendation"

**Status:** VERIFIED

### Design

Drain the tick spill automatically, so the rescue tier stops depending on a
human running curl.

ITEM 24 gave live ticks a spill tier: on a flush failure the ILP buffer is
written to `data/spill/ticks/ticks-{feed}-{hour}.ilp` instead of being
discarded. That converted a permanent in-memory loss into a replayable file —
and then stopped, because replay was a documented `curl` command. A rescue
whose recovery step needs a human fails the operator's standing
no-manual-intervention mandate, and worse, it fails it SILENTLY: the file sits
on disk looking like success.

This is not a new invention. `crates/storage/src/tick_spill_drain` existed and
was deleted in the 2026-07-17 stage-2 sweep with the rest of the dead Dhan tick
chain (`lib.rs` records it). The rescue came back in ITEM 24; its drain did not.

`tick_spill_replay` reinstates it:

- **Pure core, fully testable:** `ilp_chunk_ranges(payload, max)` splits an ILP
  body into line-aligned chunks. ILP is newline-delimited, so a chunk boundary
  that lands mid-line would corrupt two rows. A single line longer than the cap
  becomes its own oversized chunk — never split, never dropped, never
  truncated. Bounding the POST matters because a spill file may be up to
  `TICK_SPILL_MAX_BYTES` (512 MiB) and one 512 MiB body is exactly the write
  pressure that caused the spill.
- **Truncate on success, never delete.** This matches the seal precedent
  exactly: `prune_spill_files` distinguishes `deleted` (aged-out EMPTY) from
  `deleted_non_empty`, and fires `SPILL-RETENTION-01` on the latter with "the
  replay path has been broken for longer than the retention window". A drained
  file must therefore be EMPTY, not absent, or that distinction stops working.
- **Crash-safe by idempotency, not by bookkeeping.** A crash between the POST
  and the truncate re-POSTs the file next round. That is harmless because the
  `ticks` DEDUP key carries `capture_seq`, so a replayed row UPSERTs onto
  itself. No offset file, no state to corrupt.
- **A failing round stops the round.** The spill exists because QuestDB was
  already slow; hammering it with the backlog is how a bounded tick loss
  becomes an unbounded outage. First non-2xx or transport error ends the file
  AND the round.

### Edge Cases

- **Empty file** — skipped, left for the age-based pruner. Truncating an
  already-empty file every round would rewrite mtime forever and the file would
  never age out.
- **A line longer than the chunk cap** — emitted whole as an oversized chunk.
  Splitting it would corrupt a row; dropping it would be the silent loss this
  exists to end.
- **Payload with no trailing newline** — the final chunk runs to end-of-buffer.
- **Partial success** (chunks 1..k succeed, k+1 fails) — the file is NOT
  truncated and is re-POSTed whole next round. Re-writing the already-accepted
  prefix is safe (DEDUP) and is strictly better than rewriting the file
  mid-recovery.
- **Non-`.ilp` files / subdirectories** — ignored, never read.
- **Directory absent** — a no-op, not an error. A box that never spilled has no
  such directory.

### Failure Modes

- QuestDB down → every round fails, nothing is truncated, files accumulate to
  the 512 MiB cap and then the writer drops-and-counts as before. Degrades to
  exactly ITEM 24's behaviour; never worse.
- Disk full → the truncate fails; counted, and the file is re-POSTed next round.
  Duplicate writes are absorbed by DEDUP.
- **NOT fixed:** the flush timeouts themselves. Their cause is QuestDB write
  latency under live load and is untouched here. This closes the recovery gap,
  not the cause.

### Test Plan

Pure-core tests (no I/O, no QuestDB):
- chunk boundaries land only on newlines
- an over-long single line survives whole
- a payload with no trailing newline loses nothing
- chunk ranges concatenate back to the exact input (the zero-loss property)
- an empty payload yields no chunks

I/O tests against a temp dir (no QuestDB): file discovery ignores non-`.ilp`,
an empty file is skipped rather than truncated, a missing directory is a no-op.

The POST itself is NOT unit-testable without a live QuestDB and is honestly
flagged rather than faked with a mock that would prove only that the mock was
called.

### Rollback

The spawn site is one call. Removing it restores ITEM 24 behaviour exactly:
files accumulate and wait for the documented curl. No data path changes, no
schema changes.

### Observability

`tv_tick_spill_replayed_bytes_total` and `tv_tick_spill_replay_failed_total`,
both pre-registered at 0 so a first increment is not consumed as the CloudWatch
delta baseline. **FLAGGED, not fixed:** neither is EMF-selected and neither has
an alarm — the same open item ITEM 26 records. Adding a Dhan-scoped alarm needs
a dated operator row in `dhan-rest-only-noise-lock-2026-07-14.md` §2 first, and
no such quote exists, so this change deliberately does not add one.

### Per-Item Guarantee Matrix

Carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
(15-row + 7-row). Rows specific to this item:

- **Zero ticks lost** — this is the item that makes ITEM 24's rescue actually
  recover. Honest bound: rows land only when QuestDB accepts them; past the
  512 MiB cap the writer still drops and counts.
- **O(1)** — the chunker is a single forward pass over the payload, O(n) in
  bytes with no allocation per line and no backtracking; it is a cold-path
  recovery routine, never on the tick hot path. Flagged as O(n), not relabelled
  O(1), per the standing rule.
- **Extreme check** — the concatenation property test fails the build if the
  chunker ever loses or duplicates a byte.

### ITEM 27 rider — TICK-FLUSH-01 is revived, and its runbook said otherwise

The drain supervisor needs a coded error, and `TICK-FLUSH-01` already means
exactly "the off-thread tick ILP flush worker died and the supervisor respawned
it". Its original emit site was deleted in the 2026-07-17 sweep and the variant
was retained for crossref, so it was dormant rather than gone. Reusing it beats
inventing a near-duplicate whose only distinction would be *which* tick-ILP
worker died — a distinction the log line already carries in `path` and `reason`.

Reviving it meant two documents were now wrong and are corrected in the same
change: the runbook's title asserted `[RETIRED 2026-07-17]`, and the ErrorCode
docstring described only the deleted worker. Both now carry dated revival notes.
Leaving them would have been the same false-claim class this branch has been
correcting all along — a document asserting a code is dead while it pages.

**Verification:** 928 storage lib tests · 45 storage integration binaries ·
1,536 app lib tests · 65 common suites — all ok, zero failures. `cargo fmt
--check` clean; CI-equivalent clippy clean; plan-gate, banned-pattern,
data-integrity, pub-fn-test, pub-fn-wiring all PASS. The chunker's zero-loss
property is bite-proven: a one-byte off-by-one turns three tests red.

### ITEM 27 rider 2 — the banned-pattern scanner can be blinded by a brace in a string

Writing `body.find("\n}\n")` inside this module's test block made the
pre-commit gate reject ten `.expect()` calls that are all inside
`#[cfg(test)]`. The cause is in `banned-pattern-scanner.sh`: it strips test
code by tracking brace DEPTH, counting `{` and `}` per line with `gsub`, which
cannot tell a brace in code from a brace inside a string or a comment. One
unmatched `}` drops the depth to zero, the skip block ends early, and every
line after it is scanned as production.

The direction I hit is the SAFE one — it scans too much and produces false
positives, which is loud and self-correcting. The dangerous direction is the
mirror image: an unmatched `{` inside a test-block string would leave the depth
permanently above zero, and every line after it — including real production
code — would be skipped silently. That is a guard reading green because it
stopped looking.

**Surveyed, and it does not occur today.** A sweep over every `crates/*/src`
`.rs` file for "a skip block that opens and never closes while production lines
follow" returned zero hits. So this is recorded as a known fragility with a
clean current state, not an open defect.

Fixed locally by writing the brace as `\u{7d}` (which balances in source) with
a comment saying why — and the comment itself had to avoid a bare brace for the
same reason, which is a fair measure of how easy this is to trip. Hardening the
scanner to ignore braces inside string and comment tokens is a real improvement
and a separate change to a hook; it is deliberately not bundled here.

---

## ITEM 28 (added 2026-08-21) — operator: "Go ahead wirh your recommendation… check this every nook and corner with assurance and guarantee"

**Status:** VERIFIED

### Design

Four parallel audits (Rust-only, O(1)/uniqueness/dedup, hot-path allocation,
common-runtime/scalable) plus the two fixes and the alarms they justified.

**1. The spill tier becomes visible (alarms).** Authorized by the dated §2.3c row
in `dhan-rest-only-noise-lock-2026-07-14.md`. Three metrics join the EMF selector
(both copies, byte-identical); two carry market-hours-gated alarms;
`replayed_bytes` ships without one deliberately — it is the SUCCESS signal, and a
chart of recoveries makes the two failure alarms interpretable without adding a
third pager. ~$1.10/mo.

**2. A gap I invented, corrected.** The message asking for that authorization
claimed `tv_ws_frame_wal_reinjected_dropped_total` was "not EMF-selected and has
no alarm". **Both halves were false** — it is in the selector, and
`live-lane-alarms.tf:354` pages on it at threshold 1. So ITEM 26's six refusal
paths already feed a counter that ships and pages, which is a better outcome than
was claimed. Recorded in §2.3c rather than quietly enjoyed: an over-stated gap
sends the next session hunting for something that is not there.

**3. Hot path — the DHAT gate did not measure production.** Every existing gate
built a **Quote** (code 4, 50 B) packet with **no depth sink**. Production is
`FeedMode::Full` (code 8, 162 B, five depth levels) and the boot site wires
`with_inline_depth` unconditionally — so the branch doing the most per-packet work
was measured by nothing. That is the `record_ws_lag` failure mode exactly: that
path allocated twice per tick for months while three correct comments warned
against it, and a DHAT test, not a comment, is what caught it.
`full_mode_frame_with_inline_depth_does_not_allocate_per_tick` closes it.
**MEASURED: 19 allocation blocks over 10,000 ticks and 100,000 depth-row appends**
— the depth fold is allocation-free at production scale, which was previously
unknown rather than known. Ceiling set at 500, not a round 60,000: a per-tick
allocation would land at 10,000+, and a slack ceiling is a gate that passes while
the thing it guards regresses.

**4. Rust-only hole SEVEN.** `is_command_position` asked whether the prefix ENDED
WITH one of nine strings. `run: npx`, `RUN npm ci`, `ExecStart=/usr/bin/node`,
`sudo`, `exec`, `env` end with none of them — and it is the ONLY detector for
eleven runtimes, so all eleven were invisible in the dominant CI, Docker and
systemd forms simultaneously. Replaced with a prefix PARSER: the question moved
from "does it end with a known separator?" to "is the prefix entirely made of
things that precede a command?". The second has a bounded answer; the first has
the endless list that has now been wrong seven times. Live tree was and is clean.

### Edge Cases

- `echo "SSM managed node"` must NOT count — `echo` is deliberately not a command
  introducer. Six must-NOT-count fixtures pin this; the false-positive half is the
  half that matters, because a guard whose first act is a false positive teaches
  the reader to allowlist it.
- `node_modules` is not `node` — a fixture that started on the must-count list by
  mistake, where the guard was right and the test was wrong. Moved to the
  must-NOT-count list, where it is a genuinely useful assertion.
- A depth packet of all zeros could be short-circuited, so the DHAT fixture fills
  every level with non-zero values and asserts the EXACT row count
  (`FRAMES × 4 × 10`), not `> 0`.

### Failure Modes

- **HONEST LIMIT, recorded not papered over:** an env-var prefix
  (`FOO=bar node app.js`) is still not covered. After the `=` split the remainder
  is a bare word, and accepting bare words makes `managed node` a hit. A miss is a
  false negative; the alternative is a false-positive engine.
- The browser guard counts `<script` TAGS, not JavaScript — so §0.1's claim that
  it "pins browser code inside `.rs`" overstates it. Verified clean today.
- The lockfile check lists native BUILD systems, not embedded INTERPRETERS
  (`pyo3`, `mlua`, `rhai`, `deno_core`, …), which would ship an interpreter inside
  the Rust binary with nothing to detect. Verified: zero present today.

### Test Plan

- `full_mode_frame_with_inline_depth_does_not_allocate_per_tick` — measured, tight
  ceiling, exact-row-count non-vacuity.
- SCOPE FIX #10: eleven must-count forms, six must-NOT-count forms. **Bite-proven
  both ways** — deleting one introducer turns `RUN npm ci` red; restoring it green.
- EMF name-count ratchet updated 73 → 76 with a dated cost note, as that guard
  requires; it caught the change exactly as designed.

Verification: 65 common suites · 928 storage lib · 1,536 app lib · the four app
guards touching changed files — all ok. fmt clean; CI-equivalent clippy clean.

### Rollback

Every part is additive and independently revertible: the alarms are two terraform
resources plus three selector names; the DHAT gate is one test; the parser is one
function with its fixtures.

### Observability

Three metrics now ship; two page. See §2.3c for the cost line and the deliberate
no-alarm-on-success decision.

### Per-Item Guarantee Matrix

Cross-referenced to `.claude/rules/project/per-wave-guarantee-matrix.md`. Rows
specific to this item:

- **Monitoring** — the spill tier goes from log-only to shipped-and-paged.
- **Code performance** — the production packet shape is now DHAT-gated and
  measured at 19 blocks / 10,000 ticks.
- **Security hardening** — the interpreter ban now detects the invocation forms
  that dominate CI, Docker and systemd.
- **Extreme check** — three ratchets, all bite-proven; the EMF count ratchet
  proved itself by refusing the change until the cost note was written.
- **O(1)** — four new table rows, each with REACHABILITY stated, because a
  complexity number without reachability is not a decision.
