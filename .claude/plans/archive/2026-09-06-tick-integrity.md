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

- [~] **Item 2 — Make the window-gate refusals visible** (BLOCKED: operator budget decision — see Execution record)
  - `grep -rn out_of_window_refused_total deploy/` returns ZERO. Both counters exist, are seeded,
    and reach no EMF selector, no dashboard, no alarm. The size of the loss is Unknown today.
  - Add both names to `deploy/aws/cloudwatch-agent.json` and chart them. Do NOT alarm: every
    pre-open tick increments them, so an alarm would page daily.
  - Files: `deploy/aws/cloudwatch-agent.json`, `deploy/aws/terraform/dashboard.tf`
  - Tests: EMF selector lockstep guard; producer-exists guard

- [x] **Item 3 — `ARRIVAL_GRACE_TAIL_SECS` for arrival-clocked rows only**
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

- [x] **Item 5 — WITHDRAWN AS WRONG (see Execution record): deepening trades crash-durability for latency**
  - Makes the inline `perform_depth_rescue` on the drain rare without changing no-drop semantics,
    so the 2026-08-15 "nothing wiped off" lock is untouched. Cost ~192 MiB.
  - Files: `crates/storage/src/depth_persistence.rs`
  - Tests: constant ratchet with its memory derivation

- [x] **Item 6 — WITHDRAWN: already shipped 2026-09-02 (#1842)**
  - `connection.rs` drops the WHOLE frame when data is stacked ahead of a disconnect control
    packet; the code says those bytes are "gone rather than deferred". Log-only today.
  - Files: `crates/core/src/websocket/connection.rs`
  - Tests: the counter increments by the exact discarded byte count

- [ ] **Item 7 — Parked-socket auto-respawn (bounded, once per session)**
  - A fatal disconnect parks a socket permanently: "Nothing re-dials it". That shard is dark for
    the day. Breaks the no-manual-intervention requirement.
  - Files: `crates/core/src/websocket/pool_supervisor.rs`
  - Tests: respawn fires once and only once; the flap damper still bounds it

- [x] **Item 8 — Correct the stale CLAUDE.md claims**
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

---

## Execution record — 2026-09-06

Written as the work landed, including the items that turned out not to exist. Four of
the eight were sourced from a stale reading and are withdrawn; that ratio is the finding.

| Item | Outcome |
|---|---|
| 1 — blocking HTTP on the drain | **WITHDRAWN before work started.** Already fixed 2026-08-29 (#1833). The comment that claimed otherwise was written 2026-08-26 (#1824) — three days BEFORE the fix. |
| 1a — harden the spawn failure + annotate the stale comment | IN PROGRESS |
| 2 — make the window refusals visible in CloudWatch | **BLOCKED on an operator decision.** Verified: `tv_ticks_out_of_window_refused_total`, `tv_depth_rows_out_of_window_refused_total`, `tv_depth_rescue_inline_fallback_total` and `tv_depth_rescue_queued_total` each return ZERO hits under `deploy/`. Four EMF names ≈ $1.20/mo against a September forecast of $130.39 and an automatic `STOP_EC2_INSTANCES` line at $135.00 — $4.61 of margin. `dhan-rest-only-noise-lock-2026-07-14.md` §2.3n requires a LEVER, not a cost note. The approved-in-principle Elastic IP release (−$3.60/mo, Quote 10) is the lever; taking it is not an executor's call. |
| 3 — arrival grace for arrival-clocked rows | **DONE.** `ARRIVAL_GRACE_TAIL_SECS = 240`, a separate `nanos_in_any_open_window_with_arrival_grace` predicate, depth gate repointed. 7 window tests + 1 depth regression, bite-proven both directions. |
| 4 — `TV_QDB_CPUSET=3` | **NOT A FREE WIN — reclassified.** Verified: app `AllowedCPUs=1-2`, QuestDB `cpuset 2,3`, so core 2 is genuinely shared. But the host has 4 cores, core 0 services NIC softirq, and the compose file keeps QuestDB's `cpus` quota EQUAL to its cpuset width. Making them disjoint therefore takes a core from one side: QuestDB drops to 1 core (it applies WAL and merges ~1.5B depth rows/session) or the app drops to 1 (it runs the drain plus both writers). No measurement exists saying which can afford it, and guessing on a live trading box is not warranted by a 21.8%-throttling figure taken under the OLD configuration. Needs a measured comparison, not an edit. |
| 5 — depth rescue queue 2 → 8 | **WITHDRAWN as wrong.** The constant carries its own reasoning: every payload in that queue is rows that exist NOWHERE ELSE in the process, so a deeper queue converts a database stall into a bigger CRASH-LOSS window. Deepening it trades durability for latency, which is backwards for the stated priority. An existing guard (`the_depth_rescue_queue_matches_the_tick_one`) also pins parity with the tick path so the two writers cannot drift into different failure semantics. The rescue path already never drops — it falls back to an inline write — so the cost of the shallow queue is a drain STALL, not loss, and the honest fix is visibility (Item 2), not depth. |
| 6 — counter for the stacked-disconnect discard | **WITHDRAWN — already shipped** 2026-09-02 (#1842, `8f25ee625`), with the exact byte-accounting reasoning this item proposed. |
| 7 — parked-socket respawn | IN PROGRESS, narrowed. The park policy is CORRECT for pool-overflow (805) and credential rejections — the code says so and it is right: re-dialing into an 805 kills a healthy pool member. Only the transport-fatal reason is eligible, once per session. |
| 8 — correct the stale CLAUDE.md claims | **DONE.** Worse than recorded: 502,296 LoC not ~74K, 688 files not 158, 8 crates not 6, 11,750 test annotations not ~7,250. Two crates were missing from the map, including the one running the budget kill-switch. |

### The pattern, recorded because it is the reusable part

Half this plan described defects that were already fixed, or that the code had
deliberately decided against. Every one of those four was sourced from a COMMENT or from
an earlier session's note rather than from the call path. This repository already records
the same failure twice — `day_ohlc_tracker` (2026-08-12) and `WAL-SUSPEND-01`
(2026-08-25) — and the lesson did not transfer, because a plan file reads like settled
context rather than a claim. It is a claim. Verify each item against the tree at the
moment work starts on it, not at the moment it is written down.

### Closing items — 2026-09-06 (after the eight above)

**Coverage floor — DONE.** `app` ratcheted **68.3 -> 72.6**, from two independent CI
Coverage & Perf measurements on different trees (main @ `e1a1584cd` 73.07%, this PR @
`d20e43313` 72.92%). The old floor left **4.62 points** of silent-regression room — 3x
the next-widest crate. The sweep also found **nine stale floor numbers across three
live documents**, some two ratchets behind; all corrected, and
`every_documented_coverage_floor_matches_the_enforced_floor` now compares every
documented claim against the TOML so a future ratchet cannot leave the prose behind.

**Loss-counter visibility (Item 2 / Item 7) — BLOCKED, and the blocker is new.**
Three of the four counters turn out to be correctly withheld, not overlooked:

| counter | verdict |
|---|---|
| `tv_ticks_out_of_window_refused_total` | correctly withheld — a dated 2026-09-05 decision in `loss_counter_visibility_guard.rs` records that it measures the gate WORKING (outside 09:00-15:39:59 IST every tick increments it), so a series would chart normal behaviour |
| `tv_depth_out_of_window_refused_total` | same class |
| `tv_depth_rescue_queued_total` | counts normal operation, not loss |
| **`tv_depth_rescue_inline_fallback_total`** | **genuinely worth shipping** — zero on a healthy lane, and `tv_seal_escalation_inline_fallback_total` is the precedent |

So one EMF name (~$0.30/mo) is the real ask. **It cannot ship, and the reason is a live
measurement that reverses my own earlier recommendation.** Read from the account
2026-09-06 rather than inherited:

| reading | value |
|---|---|
| `limit_amount` | $150.00 |
| automatic `STOP_EC2_INSTANCES` at 90% | **$135.00** |
| September actual (6 days in) | $28.43 |
| **September forecast** | **$142.24** |
| **margin to the automatic shutdown** | **-$7.24 — ALREADY OVER** |

The repo's newest recorded figure, from ONE DAY earlier (`dhan-rest-only-noise-lock`
2026-09-05), was $130.39. It moved $11.85 in a day. Daily costs Sep 1-5 were
$8.78 / $4.79 / $5.15 / $6.10 / $3.59 — weekdays ~$4.79-8.78, weekends ~$2.79-2.87 —
so the forecast is credible, not an artifact.

`dhan-rest-only-noise-lock-2026-07-14.md` §2.3n already binds this case: *"the next
addition of any size must come with a LEVER, not just a cost note."* At a NEGATIVE
margin that is not a formality. The levers are unchanged and **neither is an
executor's to take**: the already-approved Quote 10 Elastic IP release (-$3.60/mo,
execution bundled with an instance recreate), or an operator decision on
`limit_amount` — which Quote 19 caps at $150, already the live value, so a ceiling
edit cannot buy room.

**This is the third time a constraint in this repo has expired while still being
quoted** — after the user-data byte budget and the $130 ceiling. The rule the
2026-09-02 correction wrote down applies to itself: a budget limit is one
`budgets describe-budgets` call; re-run it at the moment of writing.

---

## ARCHIVED 2026-09-06 — merged as PR #1874, squash commit `6d85b4217`

Archived per `plan-enforcement.md` rule 7. Active-plan count was at the cap of **5**
when this landed; archiving takes it to 4. That cap is not bookkeeping — the
design-first wall (`design-first-wall.md` V7) BLOCKS every implementation push past it,
and this repository has reached 107 stale active plans once before, which made the wall
vacuous because any change matched some stale plan.

**Shipped:** the depth arrival-grace fix (the one real defect), the machine-checked
park-policy table, the `app` coverage ratchet 68.3 -> 72.6 with nine corrected document
numbers and a drift guard, and the CLAUDE.md count corrections.

**NOT shipped, both operator decisions and neither started:**

| Item | Why it stopped |
|---|---|
| CPU cpuset overlap on core 2 | Real, but any disjoint layout takes a core from QuestDB or the app. Needs a measurement on the box, not a guess. |
| `tv_depth_rescue_inline_fallback_total` EMF name (~$0.30/mo) | September forecast **$142.24** against the automatic `STOP_EC2_INSTANCES` line at **$135.00** — a NEGATIVE margin. `dhan-rest-only-noise-lock` §2.3n requires a LEVER, not a cost note, and neither lever (the approved EIP release, or `limit_amount`, already at its Quote 19 cap) is an executor's to take. |

**The finding worth carrying forward:** seven of nine planned items did not exist. Every
one was sourced from a comment or an earlier session's note rather than from the call
path. Verify each item against the tree at the moment work STARTS on it, not at the
moment it is written down.
