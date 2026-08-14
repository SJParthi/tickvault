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
