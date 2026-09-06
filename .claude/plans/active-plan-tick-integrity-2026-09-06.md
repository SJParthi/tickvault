# Implementation Plan: Tick Integrity — close the drain-stall and window-gate loss paths

**Status:** APPROVED
**Date:** 2026-09-06
**Approved by:** Parthiban (operator) — "whatevr is needed and recommended go ahead and fix and
resolve everythign dude okay?", 2026-09-06 in-session, given in direct response to the
enumerated 8-item priority table reproduced as Items 1–8 below. Same authorization shape the
repo already accepts at `daily-universe-scope-expansion-2026-05-27.md` §28.2/§28.3: a general
go-ahead answering an ENUMERATED ask selects the enumerated work.

> **Slot note.** `plan-gate.sh` V7 blocks `crates/*/src/**.rs` pushes when more than
> `PLAN_GATE_MAX_ACTIVE` (5) `active-plan*.md` files exist. The tree held 4; this makes 5,
> which is AT the cap and passes ("exactly 5 plans (at cap), one valid -> PASS").

---

## Why this plan exists

A 13-agent audit (2026-09-06, three waves incl. a red-team of the audit's own conclusions)
found two live tick-loss paths that the existing guards do not catch, and confirmed that
several previously-reported items were overstated. Only the survivors are in this plan.

| Finding | Status after red-team |
|---|---|
| Blocking ILP-over-HTTP round trip on the frame drain via the inline-depth sink | **CONFIRMED — the code documents it itself** |
| Session-window gate refuses arrival-clocked rows past 15:40 | **CONFIRMED, narrowed** to the vendor-lag tail |
| `out_of_window` counters reach no EMF selector / dashboard / alarm | **CONFIRMED — the loss is unmeasurable today** |
| "Upstream loss is undetectable" | **OVERSTATED** — weakly inferable, `tick_gap_detector` already instruments it |
| "Inline rescue is unbounded" | **IMPRECISE** — bytes bounded, duration not |
| "5,142,980 ticks lost 2026-09-01" | **Assumed**, not Verified — in-source claim, no in-tree artifact |

## Guarantee matrices

This plan carries the 15-row 100% guarantee matrix and the 7-row resilience demand matrix by
cross-reference: see `per-wave-guarantee-matrix.md`. Zero ticks lost is claimed only in the
bounded form below.

**Honest claim wording.** 100% inside the tested envelope, with ratcheted regression coverage:
every frame reaching the socket is WAL-captured before parse; decode is O(1) per packet and
zero-alloc, DHAT-gated; after this plan no blocking HTTP call is reachable from the drain
task. NOT claimed: that no tick is missed. Ticks Dhan never sent are invisible and
unrecoverable — the main feed carries no sequence number.

---

## Plan Items

- [x] **Item 1 — WITHDRAWN AS WRITTEN. The defect it named was already fixed; a stale comment
  manufactured it.** Replaced by Item 1a below.
  - The original text claimed `LiveIngest::flush` leaves a blocking ILP-over-HTTP round trip on
    the frame drain via a separate, un-offloaded inline-depth sink. **Every part of that is false
    in the current tree**, and it was sourced from a code comment rather than from the call path:
    - The inline-depth sink is **not** separate. `LiveIngest::depth_sink()` records the 2026-08-28
      merge — d20, d200 and inline-d5 all write through the SAME `DepthIngest`, and a guard fails
      the build if a second one appears ("Two ILP buffers writing one table").
    - It **is** offloaded. Boot calls `ingest.spawn_depth_offload_writer()`
      (`dhan_feed_stack.rs:10025`), pinned by a guard at `:12120`; it spawns `tv-depth-writer` via
      `split_for_offload` AND `tv-depth-rescue` via `split_rescue_offload`.
    - The production flush path is `flush_and_record` → `LiveIngest::flush` → `DepthIngest::flush`
      → `DepthWriter::flush` → offload arm → `tx.try_send` — non-blocking. No HTTP and no file IO
      on the drain.
  - **Provenance of the error:** the comment at `dhan_feed_stack.rs:3920-3950` was written
    2026-08-26 (#1824); `spawn_depth_offload_writer` landed 2026-08-29 (#1833). The comment
    describes the tree as it was three days BEFORE the fix that followed it. This is precisely the
    class this repository already records twice — `day_ohlc_tracker` (2026-08-12) and
    `WAL-SUSPEND-01` (2026-08-25): **a stale comment does not merely fail to warn, it manufactures
    false findings.** It cost this session a wrong #1 priority and a wrong statement to the
    operator, both corrected on 2026-09-06.

- [ ] **Item 1a — Harden the depth-offload spawn failure, and correct the stale comment**
  - Gap A (real, and the only survivor of Item 1): on `spawn_depth_offload_writer` failure the boot
    path logs `HotPath02WriterQueueDrop` and **continues on the synchronous path**, reinstating the
    exact blocking-HTTP-on-the-drain mechanism. With depth at ~24x tick volume that fallback is not
    "degraded", it is the documented loss path.
  - Preferred: retry with backoff, then REFUSE to open sockets rather than run a lane that captures
    ticks it will lose upstream. **That trades availability for correctness and is an OPERATOR
    decision — not taken without a dated quote.**
  - Minimum, shippable now: an `AtomicBool` read at the flush site plus a degraded gauge, so the
    fallback is visible. An ALARM on it needs a dated row in `dhan-rest-only-noise-lock-2026-07-14.md`
    §3 first, per that file's own law.
  - Gap B: annotate `dhan_feed_stack.rs:3920-3950` with a dated correction block (house convention —
    annotate, never rewrite). **The `blocking_flush` wrapper STAYS**: its own comment gives the right
    reason, that gating it needs a second accessor kept in step with `LiveIngest::flush`, and that
    split knowledge is what produced the original bug.
  - Files: `crates/app/src/dhan_feed_stack.rs`
  - Tests: `the_drain_reaches_no_blocking_network_call_when_both_writers_are_offloaded` (bite-proof
    by removing `split_for_offload`); extend the existing `:12120` boot-site guard to pin the
    failure arm's disposition

- [ ] **Item 2 — Make the window-gate refusals visible**
  - `grep -rn out_of_window_refused_total deploy/` returns ZERO. Both counters exist, are seeded,
    and reach no EMF selector, no dashboard, no alarm. The size of the loss is Unknown today.
  - Add both names to `deploy/aws/cloudwatch-agent.json` and chart them. Do NOT alarm: every
    pre-open tick increments them, so an alarm would page daily.
  - Files: `deploy/aws/cloudwatch-agent.json`, `deploy/aws/terraform/dashboard.tf`
  - Tests: EMF selector lockstep guard; producer-exists guard

- [ ] **Item 3 — `ARRIVAL_GRACE_TAIL_SECS` for arrival-clocked rows only**
  - Depth carries ONE clock (`ts_nanos` IS the arrival stamp) and sentinel-LTT ticks fall back to
    the receipt. Those rows are judged on the wall clock, so a genuinely late vendor delivery past
    15:40 describing in-session activity is refused. Grace 240s, derived from the measured max
    delivery lag 198.69s.
  - Exchange-clocked ticks keep the hard 15:40 end — the operator's 2026-09-05 rule binds the
    EVENT clock; only arrival rows are asked to stamp an event they cannot see.
  - Files: `crates/common/src/constants.rs`, `crates/common/src/session_window.rs`,
    `crates/storage/src/depth_persistence.rs`, `crates/storage/src/tick_persistence.rs`
  - Tests: `arrival_verdict_accepts_the_grace_tail_and_stops_at_its_end`,
    `arrival_grace_never_admits_an_evening_restart`,
    `arrival_grace_does_not_widen_the_exchange_clock_path`, plus the depth bite-test

- [ ] **Item 4 — `TV_QDB_CPUSET=3` to end app/QuestDB core-2 contention**
  - App `AllowedCPUs=1-2`; QuestDB `cpuset 2,3`. Core 2 is shared. Measured 21.8% throttling.
  - Files: `deploy/docker/docker-compose.yml` (or the env default), `deploy/systemd/tickvault.service`
  - Tests: cpuset lockstep guard — app and QuestDB sets must be disjoint

- [ ] **Item 5 — Depth rescue queue 2 → 8**
  - Makes the inline `perform_depth_rescue` on the drain rare without changing no-drop semantics,
    so the 2026-08-15 "nothing wiped off" lock is untouched. Cost ~192 MiB.
  - Files: `crates/storage/src/depth_persistence.rs`
  - Tests: constant ratchet with its memory derivation

- [ ] **Item 6 — Counter for the stacked-disconnect frame discard**
  - `connection.rs` drops the WHOLE frame when data is stacked ahead of a disconnect control
    packet; the code says those bytes are "gone rather than deferred". Log-only today.
  - Files: `crates/core/src/websocket/connection.rs`
  - Tests: the counter increments by the exact discarded byte count

- [ ] **Item 7 — Parked-socket auto-respawn (bounded, once per session)**
  - A fatal disconnect parks a socket permanently: "Nothing re-dials it". That shard is dark for
    the day. Breaks the no-manual-intervention requirement.
  - Files: `crates/core/src/websocket/pool_supervisor.rs`
  - Tests: respawn fires once and only once; the flap damper still bounds it

- [ ] **Item 8 — Correct the stale CLAUDE.md claims**
  - Test count is 11,714 (not ~7,250); coverage floors are 63.0–99.4 (not 100%), with `app` at 68.3.
  - Files: `CLAUDE.md`
  - Tests: none — documentation truth-sync

---

## Design

Two independent loss paths, fixed independently so either can be reverted alone.

**A. Drain stall (Items 1, 4, 5).** The drain must never execute a blocking network call or an
unbounded filesystem write. Item 1 moves the last blocking HTTP call off it by giving the
inline-depth sink the same `split_for_offload` treatment the main depth and tick writers already
have. Item 5 makes the remaining inline filesystem fallback rare by deepening the hand-off queue.
Item 4 removes CPU contention so the drain and the database stop competing for core 2.

**B. Window-gate correctness (Items 2, 3).** The gate is right for exchange-clocked rows and wrong
for arrival-clocked ones. Item 2 makes the refusals measurable BEFORE changing behaviour, so the
fix can be sized. Item 3 introduces a separate `arrival_verdict` used only where the timestamp is
an arrival stamp, admitting a bounded grace tail while keeping every junk class the gate exists to
exclude.

Items 6, 7, 8 are independent and additive.

## Edge Cases

- A row arriving at exactly 15:40:00 + grace boundary — inclusive/exclusive must be pinned.
- An 18:00 mid-session restart: must still be refused (2h20m outside grace).
- Far-future `u32` epoch whose seconds-of-day land in-window: the plausible-band check runs FIRST.
- Pre-TVW3 replay with the 1970 sentinel: refusal remains CORRECT — nothing to keep.
- TVW3 replay: already survives via the preserved receipt; must not regress.
- Backlog drain: receipts are back-dated by the accept→drain dwell, so these already survive.
- Offload spawn failure at boot: must be LOUD, never a silent fallback to the blocking path.
- Shutdown tail: an extra writer thread must be joined within the existing budget.
- Depth rescue queue at depth 8: memory ceiling must stay inside the systemd `MemoryHigh=20G`.

## Failure Modes

- Item 1 spawn fails → synchronous flush returns on the drain. Mitigation: loud coded error at boot
  plus a counter; the lane still runs degraded rather than not at all.
- Item 3 grace too wide → admits genuine out-of-session events. Mitigation: 240s is derived from the
  measured max lag and is far below the nearest junk class (an evening restart).
- Item 5 raises memory → OOM risk. Mitigation: const-assert the queue byte ceiling.
- Item 7 respawn loops → redial storm. Mitigation: once per session, and the existing flap damper.
- Item 2 adds EMF cost against a thin budget margin. Mitigation: 2 names ≈ $0.60/mo, no alarm.

## Test Plan

Scoped per `testing-scope.md`: `common` changes escalate to `cargo test --workspace`; otherwise
`cargo test -p tickvault-<crate>` for storage, core and app. Every item ships a ratchet that fails
the build on regression. Bite-proof each new guard in BOTH directions — revert the fix and the test
must fail naming the defect; restore it and the suite must pass. DHAT zero-alloc tests must remain
green: no item may add an allocation to the per-tick path.

## Rollback

Each item is independently revertible. Item 4 is an env var. Item 5 is one constant. Items 2 and 8
are config/docs. Items 1, 3, 6, 7 are self-contained code changes behind their own tests; reverting
any one restores the prior behaviour exactly, and the guards revert with them.

## Observability

Item 2 is itself an observability fix. Item 1 adds a spawn-failure counter and a coded boot error.
Item 5's refusals stay on the existing `DEPTH_RESCUE_INLINE_FALLBACK_COUNTER`. Item 6 adds the
missing discard counter. Item 7 counts respawns. No new alarm is added by this plan: the budget's
September forecast is $130.39 against an automatic `STOP_EC2_INSTANCES` line at $135.00, so any new
alarm needs a lever first — that decision is Item 7 of the operator's list and is NOT taken here.
