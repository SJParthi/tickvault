# Implementation Plan: Feed Hardening — bound the limits, alarm the silence, automate the recovery

**Status:** IN_PROGRESS
**Date:** 2026-08-10
**Approved by:** Parthiban (operator) — "approve the plan", 2026-08-10 in-session, given in
direct response to the 7-item list reproduced verbatim below as Items 1–7.

> **Slot note (resolved 2026-08-10).** `plan-gate.sh` V7 BLOCKS every `crates/*/src/**.rs`
> push when more than `PLAN_GATE_MAX_ACTIVE` (5) `active-plan*.md` files exist. The tree
> was at exactly 5. The operator chose (AskUserQuestion, 2026-08-10) to archive
> `active-plan-truedata-feed.md` → `.claude/plans/archive/2026-07-24-truedata-feed.md`,
> freeing the slot for this plan. Count is back to **5** — the gate passes and app-code
> items are unblocked.

---

## Why this plan exists

An 11-agent workspace audit (2026-08-10) enumerated **86 failure scenarios** across
exchange, network, AWS, kernel, application, data-correctness and out-of-box layers.
**42 of the 86 are completely silent today** — they would cost a trading day with no
alarm. Separately, three things the operator believed were protecting the system are not:

| Believed | Measured reality |
|---|---|
| "CPU pinned to Core 0" | `core_affinity` is declared in `crates/app/Cargo.toml:47` with **ZERO call sites** |
| "Kernel/network tuned" | **No** sysctl of any kind on the prod boot path (fixed 2026-08-10, PR #1738) |
| "Memory limits sized" | `QDB_MEM_LIMIT` still defaults to `1g` — sized for the retired 4 GiB box |

This plan converts hope into bounded, tested, alarmed guarantees. It does NOT promise
"nothing will ever fail" — that wording is a REJECT per `operator-charter-forever.md` §F.

---

## Plan Items

- [x] **Item 1 — Wire CPU affinity (make the claim true or delete it)** — **DONE via the
  delete branch** (verified 2026-08-18, in source, not assumed): `core_affinity` appears
  in ZERO manifests (root `Cargo.toml` carries only a comment recording the removal, and
  no `crates/*/Cargo.toml` declares it); the charter's "core_affinity Core 0" resilience
  claim is withdrawn in `operator-charter-forever.md`; and the ratchet this item asked
  for exists and passes — `crates/common/tests/core_affinity_claim_guard.rs`
  (`core_affinity_is_wired_or_absent_never_declared_but_dead`, 3/3 green), which fails
  the build if the dependency ever returns without a call site. Ticked late: the work
  landed 2026-08-10 and the box was never checked, so an APPROVED plan carried finished
  work as pending — the same stale-state class that has manufactured false findings in
  this repo before.
  - Pin the decode/aggregation thread off the core handling network interrupts.
  - If pinning is judged wrong for a 4-vCPU shared-tenancy VM, **delete the dependency and
    the claim** instead. An unused dependency backing a documented guarantee is worse than
    no guarantee.
  - Files: `crates/app/src/main.rs`, `crates/app/Cargo.toml`
  - Tests: `test_core_affinity_is_wired_or_absent` (source-scan ratchet — the dependency
    and a call site must co-exist, or neither may)

- [ ] **Item 2 — Boot-time limit assertions (deploy + config, UNBLOCKED)**
  - Assert at boot: `net.core.rmem_max >= 134217728`, `LimitNOFILE >= 65536`,
    `vm.max_map_count >= 1048576`, and that the QuestDB memory cap matches the instance
    class. **Refuse to start** on mismatch, loudly.
  - ⚠️ **TRAP — do NOT raise `QDB_MEM_LIMIT` yet.** Quote 13 (2026-08-08) authorises 8–16 GB
    *for r8g.xlarge*, but the **live box is still t4g.medium (4 GiB)** — the r8g flip was
    REFUSED by AWS capacity (run 31148235540) and rolled back. Setting 8g on a 4 GiB box
    would OOM the database on first boot. The cap must be **derived from the detected
    instance memory**, never hardcoded.
  - Files: `deploy/docker/docker-compose.yml`, `deploy/aws/terraform/user-data.sh.tftpl`,
    `deploy/systemd/tickvault.service`
  - Tests: `boot_limit_assertion_guard.rs`

- [ ] **Item 3 — Empty-subscription + partial-subscribe detectors (scenario #75, #62)**
  — **HALF DONE 2026-08-25.** Split below rather than ticked whole: the detection
  half shipped, the auto-remediation half did not, and one tick on the parent
  would claim both.

  - [x] **3a — dead-instrument-CLASS detector (DONE 2026-08-25)**
    - Folds a per-`ExchangeSegment` liveness tally into the silence sweep that
      already runs every 30s — no second O(n) pass. A segment where every
      non-sparse instrument is still never-ticked past its warmup window emits
      `tv_dhan_feed_dead_instrument_class_total{segment}` + the
      `tv_dhan_feed_dead_instrument_classes` gauge, edge-latched once per
      episode.
    - Catches the **2026-08-21 incident directly**: 119 NSE indices subscribed,
      zero ticks all session, 8,868 tradeables flowing normally. The
      per-instrument gauge read 119-of-~9,000 — under 1.5%, indistinguishable
      from thin-instrument quiet — and nothing paged. A human found it.
    - Files: `crates/app/src/dhan_feed_stack.rs`
    - Tests: 9 in `dhan_feed_stack::tests` (7 unit on `ClassLiveness`, 1 tally
      invariant, 1 end-to-end through the live sweep).
    - **Log-sink-only.** No Telegram page: the Dhan alert family is fixed at
      four items by `dhan-rest-only-noise-lock-2026-07-14.md` §2 and a fifth
      needs a dated operator quote there FIRST. The counter and gauge are what
      an alarm would later read.
    - **Two findings recorded from bite-testing, both corrections to my own
      claims:** (1) the `pending == 0` term in `is_dead` is REDUNDANT —
      implied by `never == eligible` given the fold — so mutating it away
      changes no verdict; kept for intent, with a test pinning the invariant
      it rests on. (2) The healthy-class test does NOT guard the
      `counts_toward_alarm()` denominator trap as its first comment claimed;
      `a_single_ticking_instrument_keeps_its_class_alive` is the test that
      actually bites on it. Both comments corrected.

  - [ ] **3b — expected-vs-first-seen delta + auto-resubscribe (NOT DONE)**
    - Maintain `expected: HashSet<(sid, segment)>`; at T+120s diff against
      first-seen. Auto-resubscribe the delta on a spare slot, max 3 rounds,
      then page **with the list**.
    - **Deliberately deferred, not forgotten.** 3a is pure observation over an
      existing sweep. This half CHANGES LIVE SUBSCRIBE BEHAVIOUR — it dials a
      spare slot, so it interacts with the 16-connection budget, `plan_pool`'s
      fail-closed whole-pool refusal, and the 09:12 pre-open deadline. That
      deserves its own design pass and its own PR rather than riding along
      with a detector.
    - Files: `crates/core/src/websocket/pool_supervisor.rs`
    - Tests: `subscribe_ack_reconciliation_guard.rs`

- [ ] **Item 4 — Memory watchdog that ACTS + per-task heartbeats (#39, #57, #58)**
  - Restart at 80% memory, before the kernel's killer decides.
  - Every long-lived task emits a heartbeat; a supervisor respawns after 3 missed beats and
    counts `tv_task_respawn_total{task}`.
  - ⚠️ Note: `panic = "abort"` in release means panic-respawn arms are dead — the restart
    path must be the process supervisor, not an unwind handler.
  - Files: `crates/storage/src/oom_monitor.rs`, `crates/app/src/main.rs`
  - Tests: `task_heartbeat_guard.rs`

- [~] **Item 5 — Disk: automatic purge + raw ticks off the database (#33, #55)**
  - Raw ticks stream to compressed object storage, never bulk-ingested. At the modelled
    rate 25,000 instruments produce ~152 GB/day against a 100 GB volume — the disk fills
    in ~16 hours without this.
  - Spill rotation + purge at threshold; dead-letter size cap.
  - Files: `crates/storage/src/{disk_health_watcher,partition_archive,ws_frame_spill}.rs`
  - Tests: `disk_purge_guard.rs`

- [ ] **Item 6 — Alarms for the silent error classes (terraform, UNBLOCKED)**
  - 168 error codes exist; **23** have alarms. Add metric filters for the ~49 log-only
    classes that can cost a trading day.
  - Files: `deploy/aws/terraform/error-code-alarms.tf`
  - Tests: extend the existing paging-drift crossref ratchet

- [ ] **Item 7 — Automatic AZ failover (terraform, UNBLOCKED)**
  - On `InsufficientInstanceCapacity`, retry the **next** availability zone before paging.
    This is the exact failure that kept the box dark 2026-08-06 → 08 (Aug 5, 7, 8 = 0h CPU).
  - Files: `deploy/aws/terraform/main.tf`, `crates/aws-lambdas/src/start_watchdog.rs`
  - Tests: `az_failover_guard.rs`

- [ ] **Item 8 — Rebuild cross-verification: Dhan-only, NTM + NSE indices, and RETIRE the
  current post-market pass** — **QUEUED 2026-08-19 by operator directive** (verbatim, typos
  preserved): *"see as of now add this into the queue dude which is for cross verification
  enitlrey oen and only for ethe ntire ntm and entie idnices for dhan alone dude which is one
  an donl yfor dhan with the entire cross evrification only for entire nifty total amrket and
  entire nifty nse idncies aloen rigth ddue see do the cross evrificaiton oen and onlyh for
  these dude add this into the plan as of now totally wipe off the current post market cross
  evrification ddue okay?"*
  - **The scope, exactly:** cross-verification runs for **Dhan only**, over **the entire
    NIFTY Total Market constituent set** plus **the entire NSE index set** — and **nothing
    else**. Every other instrument class is out: no futures, no options, no BSE, no Groww,
    no bruteX-S3 leg.
  - **The retirement is half the item, not a side effect.** The operator said *"totally wipe
    off the current post market cross evrification"*. The existing pass is replaced, not
    extended — so this item DELETES its scheduler arm, its audit-table writes and its
    Telegram summary in the same change that lands the replacement. A PR that adds the new
    pass and leaves the old one running is a REJECT: two passes writing overlapping verdicts
    is precisely how the 2026-07-11 blind-since-birth comparison went unnoticed for weeks.
  - **⚠ THIS ITEM MOVES THE PROJECT'S ONLY TICK-DELIVERY MEASUREMENT — read before starting.**
    A non-zero `compared` from the 15:31 pass is the single measurement that distinguishes
    "the Dhan feed works" from "the socket is open and silent"; the India feed has **no
    snapshot-on-subscribe and no sequence number**, so packet loss is undetectable at the
    protocol level and REST comparison is the only ground truth available. Retiring the
    current pass therefore blinds that gate for exactly as long as the replacement is not
    running. **Binding consequence: the replacement must be live in the SAME change, and the
    first run must report a non-zero `compared` before this item can be called done.** A
    green build is not the completion signal here.
  - **What is genuinely better about the new scope, stated so it is not just churn:** the
    current pass compares whatever happens to be in `candles_1m`, so a shrinking universe
    silently shrinks the comparison and still reports "no mismatches". Pinning the set to
    NTM + NSE indices makes the DENOMINATOR explicit — a missing constituent becomes a
    `missing_live` row naming the instrument, instead of a smaller quiet pass.
  - **The known trap this item must not repeat (Verified, PR #1474, 2026-07-11):** the last
    implementation was **BLIND SINCE BIRTH** — the `candles_1m` side used NANOSECOND literals
    against QuestDB's MICROSECOND timestamp comparison, so the WHERE window sat near year
    58502, matched zero rows on every run since the feature shipped, and reported
    `compared=0` honestly while nobody read it. The replacement needs a digit-magnitude
    assertion on its own SQL literals, and a `compared == 0` verdict must classify **Blind
    (High)**, never `Ok`.
  - **Instrument sourcing (must self-roll, no hardcoded list):** the NTM constituents come
    from the niftyindices list joined to the Dhan master by **ISIN** — the join is already
    specified and locked in `daily-universe-scope-expansion-2026-05-27.md` §31.1 (ISIN
    primary, `(Symbol, Series=EQ)` cross-check, symbol-alone BANNED, O(1) `HashMap` build,
    unresolved constituents COUNTED and LOGGED BY NAME, 2% membership tolerance kept
    separate from the 0.5% F&O dangling tolerance). Build to that contract; do not reinvent
    it. The NSE index set comes from `NSE_INDEX_ALLOWLIST` + `canonicalize_index_symbol`.
  - **Rule-file-first:** the scope change must be recorded with this dated quote in
    `no-rest-except-live-feed-2026-06-27.md` (the §8 Dhan REST grant that feeds the
    comparison) and in the cross-verify runbook BEFORE the code lands, and the
    Groww/bruteX-side cross-verify sections annotated as superseded-in-place per house
    convention — never rewritten.
  - Files: `crates/app/src/cross_verify_*`, `crates/storage/src/*crossverify*`,
    `crates/core/src/instrument/` (the ISIN join), the scheduler arm that fires the pass
  - Tests: digit-magnitude SQL literal guard; `compared == 0` ⇒ Blind classification;
    ISIN-join fail-closed + unresolved-named; a ratchet proving the OLD pass has no
    remaining scheduler call site (so the retirement cannot half-land)

- [ ] **Item 9 — depth-200 = ATM CE/PE of the current expiry, NIFTY + BANKNIFTY only, with a
  HYSTERESIS re-subscribe policy** — **QUEUED 2026-08-19 by operator directive** (verbatim,
  typos preserved): *"see emanwhiel as of now for depth 200 always stick to atm ce pe of
  ciurrent expiry aloen for both nifty and banknifty dude okay? see how will yo ualways ensrue
  that see everytiem it needs ti be resusbcriebd as per the atm rigth ddue do you udnerstdn
  what im aksign dude see is ti beeter to go ahead wirh atm resusbsitption alwats or jsut stick
  tot eh entire day of the curretn day starting seocdn mintue atm as static tll eod dude
  okay?"*
  - **The set is exactly 4 instruments** — NIFTY CE, NIFTY PE, BANKNIFTY CE, BANKNIFTY PE, all
    at the ATM strike of the current expiry. depth-200 permits **1 instrument per connection**,
    and 5 connections are authorized, so this uses 4 and leaves 1 spare. It fits the socket
    budget exactly; no arithmetic is being stretched.

  - **THE ANSWER to "static or always re-subscribe": NEITHER — hysteresis.** Both options as
    posed have a real defect, and the third shape is the one this repo already used before the
    depth feeds were retired:

    | Policy | What it gets right | Why it is wrong on its own |
    |---|---|---|
    | **Always re-subscribe on ATM change** | the book always describes the strike where liquidity actually is | every swap is `unsubscribe(25)` + `subscribe(23)` on that socket, so the book has a HOLE at exactly the moment of a fast move — the moment the depth is most worth having. On a trending day this churns repeatedly and the day's series is a stitched sequence of fragments, not one book |
    | **Static from the 2nd minute to EOD** | one contiguous 200-level book per instrument, perfectly comparable all day, zero churn, zero gaps | a 2% index move leaves the "ATM" strike deep OTM by the close. Its book thins to almost nothing, so the back half of the day records depth for a strike nobody is trading — technically complete data about the wrong instrument |
    | **HYSTERESIS (recommended)** | keeps the book on a strike that stays meaningful, while swapping rarely enough that the series stays readable | needs two named constants and a dwell timer — which is work, not a config flip |

  - **The hysteresis contract to build:** select ATM at the 2nd minute (09:16, once the first
    real prints exist — the pre-open cross can print a spot that is not the trading spot);
    thereafter re-evaluate on a slow timer, and swap **only** when spot has drifted at least a
    named threshold of strikes from the subscribed strike **and** the current subscription has
    been held at least a named minimum dwell. Both thresholds are constants with their own
    tests, never literals at the call site. Every swap writes an audit row naming the old
    strike, the new strike, the spot that triggered it, and the exact instant — so the hole in
    the book is explicit in the data rather than something an analyst has to infer from a gap.
    This is the shape the deleted `depth_rebalancer` used (60s spot check, swap on ≥3 strike
    drift, command-channel swap with **no disconnect**), and reusing it is deliberate: the
    command-channel form is what makes a swap `unsubscribe`+`subscribe` on the SAME live
    socket rather than a reconnect.

  - **⚠ SEQUENCING — do NOT open a depth socket first.** The operator's own 2026-08-15 second
    quote binds this: *"either the vertical lands, or the depth pools stay at zero
    instruments"*, and today `ls crates/storage/src | grep -i depth` returns **nothing** —
    depth-200 frames are pulled at 512 KiB each and every one is discarded. The order is
    therefore: **(1)** writer + DDL + dedup key (with the `d20`/`d200` discriminator the same
    quote requires, or the two pools silently overwrite each other) + same-day S3 archival →
    **(2)** the 4-instrument ATM set → **(3)** the hysteresis policy. Opening sockets before
    (1) means paying 512 KiB/frame to discard, and reporting them as "connected" is the
    false-OK the scope lock forbids.

  - **What makes the strike resolvable at all:** the ATM strike needs a live spot for
    NIFTY/BANKNIFTY (the main feed carries both) and a current-expiry contract
    `security_id` for that strike. The ONLY authorized source for the latter is the
    per-minute option-chain pull's per-leg `contract_security_id` (2026-08-11 second quote) —
    it self-rolls at expiry, costs no new fetch class, and a hardcoded contract list is a
    REJECT because it goes stale weekly. A `contract_security_id` of `0` must be REFUSED and
    counted, never subscribed: the parser defaults it to 0 when the field is absent, and
    subscribing instrument 0 would look healthy while carrying nothing.

  - **First live session should run STATIC anyway** — not as the policy, as the bring-up.
    Depth-200 has never delivered a single packet to this system. Proving the socket connects,
    the 200-level frames parse at the right offsets, and the writer persists them is a strictly
    easier problem with the instrument held still. Turn hysteresis on once a static session has
    produced verified rows. Recorded so "static" is understood as a bring-up step with an exit
    condition, not as the answer.
  - Files: `crates/storage/src/depth_persistence.rs` (new), `crates/app/src/dhan_feed_stack.rs`
    (the depth arm that currently discards), `crates/core/src/instrument/` (ATM selection)
  - Tests: dedup key carries the depth-kind discriminator; ATM selection at a boundary strike;
    hysteresis does NOT swap inside the dwell window; a swap emits its audit row; a `0`
    contract id is refused and counted

- [x] **Item 10 — 09:15 first-bucket open/high/low (operator 2026-08-19: "Yes go ahead only for this 9.15 am open high low alone dude okay?")**
  - Fix the live defect where the day's first candle can publish `open` OUTSIDE its own
    `[low, high]` (gap-open mornings): clamp the range to contain the exchange official
    open at BOTH `day_open` stamp sites.
  - Adopt `tick.day_high` / `tick.day_low` as the exchange-true extremes for the day's
    FIRST bucket ONLY — monotone widening, `is_finite() && > 0.0` gated.
  - Scope guard: buckets after the first are UNTOUCHED (a running session extreme does not
    describe a later bucket). The D1 whole-day change is explicitly NOT in scope.
  - Files: `crates/trading/src/candles/aggregator_cell.rs`
  - Tests: `first_bucket_open_below_ltp_still_yields_valid_ohlc`,
    `first_bucket_open_above_ltp_still_yields_valid_ohlc`,
    `late_day_open_into_folded_bucket_keeps_ohlc_valid`,
    `first_bucket_adopts_exchange_day_high_and_day_low`,
    `zero_day_high_low_sentinel_is_never_adopted`,
    `non_finite_day_high_low_is_never_adopted`,
    `second_bucket_ignores_running_day_extremes`,
    `widening_never_narrows_an_observed_range`,
    `ohlc_invariant_holds_across_a_folded_session`,
    `day_boundary_rearms_the_first_bucket_treatment`
  - Full design: ITEM 10 addendum below (C1–C8)


---

## Design

Every item follows one principle: **replace an unbounded hope with a bounded, asserted
limit plus an alarm that fires when the bound is crossed.** No item introduces a new
promise; each converts an implicit assumption into an explicit, testable constant.

Ordering is by blast radius, not effort. Item 7 first (nothing works while the box is
dark), then Item 2 (a misconfiguration must never reach market open), then Item 3 (the two
scenarios that silently cost a whole day), then 5, 4, 6, 1.

Boundaries respected: no change to the `§28` indicator/strategy frozen area; no change to
the 4-gate order-fire lattice; `dry_run` stays true; no new WebSocket endpoint beyond the
16 authorised 2026-08-09.

## Edge Cases

- Instance memory smaller than the configured database cap → derive, never hardcode (the
  live-box trap above).
- Boot assertion fails on a **legitimately** different instance class → assertion must read
  the class, not a constant, or it becomes a boot-blocker on a valid machine.
- Empty-subscription detector during a genuine market halt → must not force-cycle when the
  whole exchange is silent; gate on halt state.
- Partial-subscribe resubscribe racing a reconnect → cap at 3 rounds, then page.
- AZ failover racing the daily start cron → idempotent, single-flight.
- Disk purge deleting data that has not yet been archived → archive → verify → drop, never
  drop first.
- Heartbeat supervisor starved by the same pressure that killed the task → the supervisor
  must not depend on the starved runtime.

## Failure Modes

| Mode | Consequence | Mitigation |
|---|---|---|
| Boot assertion too strict | Box never starts | Assertions warn for one release, then enforce |
| Memory watchdog restarts in a loop | Session lost | Restart budget: max 3/session, then page and stay up |
| Auto-resubscribe storm | Vendor rate-limits us | Hard cap 3 rounds, backoff, count |
| AZ failover thrash | Instance replaced repeatedly | Single-flight lock + one AZ attempt per cron tick |
| Purge deletes live data | Data loss | Archive→verify→drop ordering, never reordered |
| New alarms cause page fatigue | Operator ignores pages | Edge-triggered only, market-hours gated |

## Test Plan

Categories per `testing.md`: 3 (error scenario), 4 (edge case), 5 (stress/boundary),
10 (backpressure), 12 (graceful degradation), 13 (panic safety), 22 (integration).
Each item ships a **build-failing ratchet** named above. DHAT allocation gates added for
`consume_tick` and `append_row` — currently the two hottest paths have **no** allocation
gate, so their zero-alloc claims are prose, not enforcement.

Every claim in the resulting PRs must paste real output. Unmeasured outcomes are stated as
unmeasured (`zero-loss-guarantee-charter.md` §4).

## Rollback

Every item is independently revertible. Items 2, 6, 7 are configuration/infrastructure —
revert is a terraform apply of the prior state. Items 1, 3, 4, 5 are behind either a config
flag defaulting OFF or a constant that can be restored. No item changes a persisted schema,
so no data migration is required and rollback cannot orphan rows.

The kernel tuning already shipped (PR #1738) applies on fresh-instance boot only; rolling it
back is deleting one file under `/etc/sysctl.d/` and rebooting.

## Observability

Each item adds: a counter, a log line carrying a typed error code, and — where the failure
can cost a trading day — a CloudWatch alarm. Specifically:
`tv_task_respawn_total{task}`, `tv_subscribe_reconcile_missing_total`,
`tv_ws_rcvbuf_used_bytes`, `tv_boot_assertion_failed_total`,
`tv_disk_purge_bytes_total`, `tv_az_failover_attempt_total`.

Per-item guarantee matrix: `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row +
7-row), carried in each implementing PR body.

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: bounded buffers
(200,000-seal ring → NDJSON spill → DLQ), zero-allocation parse, pre-allocated slots that
never grow mid-session, and fail-closed refusals on every limit.

**NOT claimed:** that nothing will impact the system. CPU steal is hypervisor-owned, the
memory killer is kernel-owned, and the 87th failure scenario is unknown by definition.
**NOT claimed:** "never miss a tick" — the vendor protocol has no sequence number, no
replay and no snapshot-on-subscribe, and measured reconnect windows of 7–11 s lose data at
source. The honest guarantee is **capture-completeness of received frames**, never
trade-completeness.

---

## ITEM 5 — DESIGN ADDENDUM (added 2026-08-19, operator: "yes go ahead dude but see clealry ensure that we shdou lneevr ever miss any ticks or websocket disconenction or reocnenction or no cnaldes loss no ticks liss ntohgin bro okay? … nothign shdou lbe missed or dleetd dude yous aid yo uwill put all thos into somwhere isnetad if our disk stroage right dude am ii rght dude tell me dude okay?")

The operator's second sentence is a CONFIRMATION QUESTION, and the answer is yes: the
data goes to **S3**, not to a delete. That is what this addendum builds a trigger for,
and every choice below is subordinate to the first sentence — nothing missed, nothing
deleted.

### B1. What already exists, and is NOT being rebuilt

`crates/storage/src/partition_archive.rs` already implements the whole dangerous part,
fail-closed and in this order: export the partition to gzipped CSV → `HeadObject`
never-overwrite check → conditional `PutObject` with a server-validated SHA-256 →
verify (row-count re-query AFTER export, object exists, ContentLength matches, record
count matches) → append the `verified` audit row and FLUSH it → only then `ALTER TABLE
DROP PARTITION`. A `VerifiedArchive` type-state makes "drop without a verified copy"
unrepresentable rather than merely unlikely.

**This addendum adds no delete path.** It adds a *trigger* that calls that same
function. That distinction is the whole safety argument: anything the pressure path
drops has already been proven byte-present in S3 by code that predates this change and
is already ratcheted.

### B2. The defect (Verified in source, 2026-08-19)

| Fact | Evidence |
|---|---|
| Archival runs ONCE per day, post-market | `crates/app/src/main.rs:3064`, inside the post-close block after the drain sleep |
| Eligibility is AGE only | `hot_window_days()` → 35 (market data) / 3 (depth) / 90 (standard), clamped to `MIN_HOT_DAYS = 2` |
| The disk watcher never acts | `disk_health_watcher.rs` publishes `tv_spill_dir_free_bytes` and logs; it has no remediation arm |

At the authorized scale the modelled tick volume is ~152 GB/day against a 100 GB volume
— the disk fills in **~16 hours**, which is INSIDE the 2-day minimum eligibility window
and hours before the post-market run. **The existing cleanup can never fire in time.**
A full volume stops every writer — ticks, candles, depth, audit — so the failure mode is
not "old data lingers", it is "today's capture stops", which is the total-loss class the
operator's first sentence forbids.

### B3. The three constraints that shape the design (each one costs something)

1. **`MIN_HOT_DAYS = 2` stays inviolate — today and yesterday are never eligible, at any
   pressure.** The verify step re-counts AFTER the export, which closes the
   export→count race; it does NOT close the count→drop race. On a partition that is
   still receiving writes, a tick landing in that window would be dropped with the
   partition. That is a one-tick loss, and one is too many. **Cost:** the floor means
   pressure archival cannot reclaim today's or yesterday's bytes — if two days of data
   alone exceeds the volume, no retention policy can help, and §B6 says so out loud
   instead of deleting.
2. **Pressure NEVER escalates into deletion.** When everything eligible has been
   archived and the volume is still above the high-water mark, the loop stops and fires
   a Critical coded error. A system that deletes unarchived data to save itself has
   converted a disk problem into a data-loss problem.
3. **Bounded and hysteretic.** One pass at a time, a cooldown between episodes, a
   max-passes cap, and a low-water exit that is strictly below the high-water entry —
   so a volume hovering at the threshold cannot thrash QuestDB with export queries
   during the session.

### B4. Design

**New config (`PartitionRetentionConfig`), all serde-default OFF/inert:**

| Key | Default | Meaning |
|---|---|---|
| `pressure_archive_enabled` | `false` (serde) | master gate; base.toml opts in |
| `pressure_high_water_pct` | 75 | at/above this used-%, an episode starts |
| `pressure_low_water_pct` | 60 | below this used-%, the episode ends |
| `pressure_hot_days` | 2 | hot window used ONLY under pressure, still clamped to `MIN_HOT_DAYS` |
| `pressure_min_interval_secs` | 900 | cooldown between episodes |
| `pressure_max_passes` | 4 | passes per episode before escalating |

**A pure decision function** — `decide_pressure_action(probe, state, cfg) -> PressureAction`
— so every branch is unit-testable with no disk, no QuestDB and no S3:
`Idle` · `StartEpisode` · `ContinueEpisode` · `EndEpisode` · `Escalate` · `Cooldown`.
The loop that calls it does I/O only.

**Wiring:** one supervised task in the app crate (the house respawn pattern), polling the
QuestDB data volume; on `StartEpisode`/`ContinueEpisode` it constructs the existing
`PartitionArchiver` with `market_data_hot_days`/`depth_hot_days` overridden to
`pressure_hot_days` and calls `archive_and_drop_old_partitions()` unchanged.

### B5. Edge Cases

| # | Case | Behaviour |
|---|---|---|
| 1 | Volume above high-water at boot | Episode starts on the first poll — no warm-up grace, because a full disk is already losing writes |
| 2 | `df` probe fails | Counted, logged, treated as **Idle** — never as pressure. A blind probe must not trigger drops |
| 3 | Nothing eligible (all partitions < 2 days) | `Escalate` — Critical coded error, ONE per episode (edge-latched), loop keeps polling but takes no destructive action |
| 4 | Archive pass fails (S3 down, verify mismatch) | The existing path keeps every partition; the pass counts as used; after `pressure_max_passes` → `Escalate` |
| 5 | Pressure clears between passes | `EndEpisode` on the first probe below low-water; latch resets so the next episode can page again |
| 6 | Volume hovers exactly at high-water | Hysteresis: exit requires `< low_water`, which is strictly below entry, so no thrash |
| 7 | Post-market daily run overlaps a pressure episode | Both call the same idempotent function; a partition already dropped is simply not listed the second time |
| 8 | `pressure_archive_enabled = false` | The task is not spawned. Byte-identical to today |

### B6. Failure Modes

- **Two days of data exceeds the volume.** Unfixable by retention, by construction of
  the §B3.1 floor. Behaviour: `Escalate`, Critical, loud, no deletion. The remedy is an
  operator decision (grow the volume — gp3 grows online in one command — or reduce
  ingest scope), never an executor one.
- **S3 unreachable during pressure.** No drops occur (verify cannot pass). The disk
  continues filling and the escalation fires. Correct: an unverifiable copy is not a copy.
- **Export load during the session.** Bounded by `pressure_max_passes` and the cooldown;
  only partitions ≥2 days old are read, so the export never touches the partitions the
  live writers are appending to.

### B7. Test Plan

Unit (pure, no I/O): every `PressureAction` branch incl. hysteresis, cooldown,
max-passes escalation, probe-failure-is-Idle, and the `MIN_HOT_DAYS` clamp surviving a
`pressure_hot_days = 0` config. Ratchet (`disk_purge_guard.rs`): the pressure path calls
`archive_and_drop_old_partitions` and NOT any `DROP PARTITION` of its own — bite-proven
in both directions; `MIN_HOT_DAYS` is still 2; the escalation arm carries a coded error.
Config: an absent `[partition_retention]` pressure block deserializes to disabled.

### B8. Rollback

`pressure_archive_enabled = false` restores today's behaviour exactly — the task is not
spawned and no other code path changes. The config keys are additive with serde
defaults, so an older binary reading a newer config, or the reverse, both work.

### B9. Observability

`tv_data_disk_used_pct` (gauge) · `tv_disk_pressure_episodes_total` ·
`tv_disk_pressure_passes_total` · `tv_disk_pressure_partitions_dropped_total` ·
`tv_disk_pressure_unrelievable_total` · `tv_disk_pressure_probe_failed_total`.
Escalation logs `STORAGE-GAP-05` (new, Critical) once per episode.

### B10. Honest envelope

**Claimed:** with the pressure trigger on, local disk usage is bounded to the pressure
hot window (floor 2 days) *provided two days of data fits the volume*, and every byte
that leaves local disk has a checksum- and row-count-verified S3 copy first.

**NOT claimed:** (a) that this makes a full disk impossible — if two days exceeds the
volume it escalates and stops, by design; (b) that "raw ticks never enter the database"
in the Item 5 headline sense — they still land in QuestDB and leave via the verified
archive, which is *how* they get off local disk; re-plumbing ingest to write object
storage directly would break the `ticks` contract, the materialized views and the
cross-verification, and is not attempted here; (c) any measurement at 25,000 instruments
— the ~152 GB/day is arithmetic from row widths, and the trigger has never run against a
real filling volume; (d) "never miss a tick" in the trade-completeness sense — the
vendor protocol has no sequence number, no replay and no snapshot-on-subscribe, so the
guarantee remains capture-completeness of RECEIVED frames.

---

## ITEM 10 — DESIGN ADDENDUM (added 2026-08-19, operator: "Yes go ahead only for this 9.15 am open high low alone dude okay?")

Given in direct response to a finding that the day's FIRST candle can be published with
its `open` OUTSIDE its own `[low, high]` range. Scope is exactly what the quote says:
the day's first bucket's open / high / low. The D1 whole-day change discussed in the
same exchange is explicitly NOT taken.

### C1. The defect (Verified in source, 2026-08-19)

`open_bucket` stamps the exchange official open but seeds the range from the LTP:

| Site | Line | What it does | Invariant safe? |
|---|---|---|---|
| Bucket open, armed | `aggregator_cell.rs:458` | `open = day_open`, `high = low = ltp` | ❌ **NO** |
| Late `day_open` into an open bucket | `aggregator_cell.rs:483` | `slots[ord].open = day_open` on an already-folded bucket | ❌ **NO — worse: high/low may be far away by then** |
| Intraday roll | `aggregator_cell.rs:502` | `use_day_open = false` → `open = high = low = ltp` | ✅ yes |

Concrete failure, gap-up morning: pre-open equilibrium `day_open = 100`, first traded
`ltp = 105` ⇒ published candle is `open=100, high=105, low=105` — **the open is below
the low**. A workspace scan for any `open <= high` / `open >= low` style invariant
returns ZERO matches: nothing anywhere catches it.

Consumers this corrupts: ATR/true-range legs, any typical-price indicator, every
charting renderer (body outside its own wick), and the worst-case-fill backtest rule
(fills at a range that never contained the open).

### C2. Design

Two changes, both **monotone widening only** — they can enlarge `[low, high]`, never
shrink it. That property is the whole safety argument: no input, however corrupt, can
narrow a range or discard an observed extreme.

**(a) Open clamp — unconditional, at both `day_open` stamp sites.**
After `open` is set from `day_open`, widen the range to contain it:
`high = max(high, open)`, `low = min(low, open)`. Justified because the pre-open
equilibrium is a REAL matched trade, so it genuinely belongs inside the bar's range.
Needs no new state — it applies exactly where `day_open` is stamped, which the existing
arm already restricts to the day's first bucket.

**(b) Exchange extremes on the day's FIRST bucket only.**
Widen with `tick.day_high` / `tick.day_low`: `high = max(high, day_high)`,
`low = min(low, day_low)`. Correct ONLY for the first bucket, because `day_high` is a
running SESSION extreme — for any later bucket it describes the whole day, not that
bucket. That is precisely the operator's scope.

**First-bucket signal — derived, zero new state:** `last_sealed[ord].is_uninitialised()`.
Nothing has sealed for that timeframe yet today. `force_seal` (day boundary) clears
`last_sealed` and re-arms, so it resets correctly across days. Chosen over a new
`[bool; TF_COUNT]` array because it costs 0 bytes and cannot drift out of sync with the
seal path it is derived from.

**Cost — deliberately kept off the 99.7% path:** `day_high`/`day_low` are read from
`&ParsedTick` (already in hand at both sites) and widened inline ONLY when the
first-bucket test passes. `TickPrices` is deliberately NOT extended: adding two f64
fields there would pay ~100 ns of `f32_to_f64_clean` on EVERY tick of the session to
serve one bucket per day. As designed the conversion runs during the first bucket only —
roughly one minute out of 375 — and costs literally zero for the rest of the session.

**Validity gate:** `v.is_finite() && v > 0.0`. `> 0.0` alone is insufficient —
it rejects NaN (`NaN > 0.0` is false) but ACCEPTS `+∞`, which would set `high` to
infinity permanently. `is_finite()` closes both.

### C3. Edge Cases

| # | Case | Behaviour |
|---|---|---|
| 1 | Ticker mode — `day_high/day_low = 0.0` (ABSENT sentinel) | Gate rejects; range untouched. **Never treated as a price of zero** |
| 2 | `day_high = NaN` | `is_finite()` rejects |
| 3 | `day_high = +∞` | `is_finite()` rejects — the case `> 0.0` alone would have let through |
| 4 | `day_low = 0.0` on a real instrument | Rejected — cannot drag a low to zero |
| 5 | `day_high < ltp` (stale/lagging field) | `max` keeps the ltp — widening only |
| 6 | `day_low > ltp` | `min` keeps the ltp — widening only |
| 7 | `day_high < day_low` (corrupt pair) | Each applied independently by max/min; range can only widen. No inversion possible |
| 8 | `day_open` inside `[low, high]` already | Clamp is a no-op |
| 9 | Pre-open auction included in `day_high` | First bar spans pre-open — CONSISTENT, because its `open` is the pre-open price too |
| 10 | Pre-open NOT included | First bar spans 09:15 only — also consistent |
| 11 | First tick arrives 09:47 (illiquid) | Keys on first bucket OPENED, not on clock 09:15 — correct by construction |
| 12 | `catch_up_seal` drains the first bucket, then a late tick arrives | `last_sealed` now set ⇒ widening skipped. Safe: the widening already ran while the bucket was live |
| 13 | Day 2 of the process | `force_seal` clears `last_sealed` + re-arms ⇒ signal resets |
| 14 | Instrument never ticks | Slot uninitialised, emits nothing — sparsity preserved |
| 15 | Intraday bucket (09:16 onward) | `last_sealed` initialised ⇒ untouched. **The scope guarantee** |

### C4. Failure Modes

| Mode | Blast radius | Containment |
|---|---|---|
| Corrupt `day_high` widens a bar too far | One instrument, one bucket, one day | Widening-only: an observed trade is never discarded; the bar stays a superset of truth |
| Sentinel `0.0` adopted as a low | Would be catastrophic | Structurally impossible — `> 0.0` gate, ratcheted by a dedicated test |
| `+∞` adopted as a high | Would poison the bar | `is_finite()` gate, ratcheted |
| Derived first-bucket signal drifts | Widening applied to a later bucket | Signal is READ-ONLY off `last_sealed`, which only the seal path writes; a test pins that bucket 2 is untouched |
| Hot-path regression | All instruments | Conversion is first-bucket-only; DHAT + Criterion budgets unchanged |

### C5. Test Plan

| # | Test | Pins |
|---|---|---|
| 1 | `first_bucket_open_below_ltp_still_yields_valid_ohlc` | The gap-up bug: `open=100, ltp=105` ⇒ `low <= open <= high` |
| 2 | `first_bucket_open_above_ltp_still_yields_valid_ohlc` | Gap-down mirror |
| 3 | `late_day_open_into_folded_bucket_keeps_ohlc_valid` | Site 2 (`:483`) — the worse one |
| 4 | `first_bucket_adopts_exchange_day_high_and_day_low` | Operator idea #2 works |
| 5 | `zero_day_high_low_sentinel_is_never_adopted` | Ticker-mode `0.0` |
| 6 | `non_finite_day_high_low_is_never_adopted` | NaN **and** `+∞` |
| 7 | `second_bucket_ignores_running_day_extremes` | **The scope guarantee** — 09:16 untouched |
| 8 | `widening_never_narrows_an_observed_range` | Monotonicity: `day_high < ltp` cannot shrink |
| 9 | `ohlc_invariant_holds_across_a_folded_session` | Property-style sweep over a synthetic session |
| 10 | `day_boundary_rearms_the_first_bucket_treatment` | Cross-day reset |

### C6. Rollback

Pure revert — the change is confined to `aggregator_cell.rs`, adds no field, no config
key, no table, no migration, and no wire format. Reverting restores byte-identical
prior behaviour. No data written under the new code needs repair: every bar it produces
is a superset-range version of what the old code produced, and the DEDUP key is
unchanged so a re-fold UPSERTs in place.

### C7. Observability

Two counters, both first-bucket-only so cardinality and volume are trivially bounded:

| Metric | Meaning |
|---|---|
| `tv_candle_open_clamped_total` | Times the official open fell outside the LTP-seeded range — i.e. **how often the live bug would have fired** |
| `tv_candle_day_extreme_adopted_total{side="high"\|"low"}` | Times an exchange extreme genuinely widened the first bar |

Deliberately NO new `ErrorCode` and NO alarm: neither event is a fault. A clamp is the
NORMAL gap-open case, and paging on a normal morning is the alert-fatigue class this
repo has retired before. The counters exist to MEASURE, and the first live session's
value for the first counter is the honest evidence of how real the defect was.

### C8. Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: the OHLC invariant
`low <= open <= high` holds for every bar the aggregator emits, proven by unit tests at
both `day_open` stamp sites plus a folded-session sweep; the exchange extremes are
adopted for the day's FIRST bucket only, pinned by a test asserting bucket 2 is
untouched; and every adoption is monotone-widening, so no input can narrow a range or
discard an observed trade.

NOT claimed: (a) that the first bucket's high/low is EXACT — it is exact only if Dhan's
`day_high`/`day_low` are populated and fresh on the tick that closes the bucket, which
is **UNVERIFIED-LIVE** (no Dhan tick confirmed received since the 2026-07-13
retirement; the 15:31 cross-verify reporting non-zero `compared` remains the only
proof); (b) that any bucket AFTER the first gains accuracy — none does, by design;
(c) that intra-second trades Dhan conflated upstream are recovered — they are lost at
source and no field can return them; (d) whether Dhan's `day_high` includes the
09:00–09:15 pre-open auction — Dhan does not document it, `docs/dhan-ref/` was searched,
and both answers are handled correctly by construction (C3 rows 9–10).

---

## ITEM 11 — DESIGN ADDENDUM (added 2026-08-23, operator: "see now fix and resoleve and emreg and deploy evrythign dude okay?")

Two findings from the 2026-08-23 workspace sweep. Both belong to this plan rather than
a new one: the CPU claim is already in this plan's own "Believed vs Measured" table, and
the price finding is the data-correctness layer this plan's audit enumerated. Neither is
a live defect; both are latent, and both were left for the operator until this quote.

### 11a — Worker threads are pinned to one machine; memory is not

**Design.** `DEFAULT_TOKIO_WORKER_THREADS = 2` is a constant chosen for the r8g.xlarge's
4-core partition. Nothing in the workspace reads the host's CPU allowance — no
`/sys/fs/cgroup/cpu.max`, no `available_parallelism()`. Memory got the derive-from-host
fix twice (`market_ram_store_boot` reads `/proc/meminfo` AND the cgroup limit and takes
the min; the frame ring reads `/proc/meminfo` for 2%). CPU never did.

Derive the default the same way: read the cgroup CPU quota, fall back to
`available_parallelism()`, clamp to `[1, 4]`. **On the prod box this must still yield 2**
— the systemd unit confines the process to a 2-core cpuset, so the quota reads 2 and the
operator's Quote 17b value is preserved exactly. The env override stays the rip-cord.

**Why it is not a Quote-17b reversal.** 17b authorized "an explicit tokio worker-thread
count" as part of CPU isolation. Deriving from the cpuset that the isolation itself sets
IMPLEMENTS that intent on any host, rather than hard-coding the answer for one host.

### 11b — A price narrowed to f32 for a wire format that no longer exists

**Design.** `MarkUpdate.price` is `f32`, documented as "f32 as delivered — the runtime
widens for math". That premise is dead: the same struct's doc records the per-tick
live-bridge source dying with #1581, and both remaining callers are REST legs delivering
`f64` (`candle.close`; `leg.last_price`, parsed by `val_f64`). Both narrow with `as f32`
at the call site. The far end, `update_market_price_in_segment`, takes `f64` — and its
own doc says it "feeds `daily_loss_state`, which feeds the daily-loss auto-halt".

So the chain is f64 → f32 → f64 on the kill-switch input, for no wire.

Widen the field to `f64`, delete both casts, drop the now-pointless `f32_to_f64_clean`
round trip, and correct two stale doc claims on the same struct (the "as delivered"
premise, and a `security_id` doc still describing the removed Groww token space).

**Honest magnitude.** NOT a live defect. The widening is currently done correctly
(`f32_to_f64_clean`, not the banned `f64::from`), and every price that flows is below the
cliff. The cliff is computable: f32's step is 0.0078 below 131,072 (finer than a paisa)
and 0.0156 above it (coarser). Nothing forwards an equity mark today; the universe was
just widened to ~24,600 instruments, and the next change that forwards one crosses that
line with no error, no counter, and no comment saying the line exists.

## Edge Cases (Item 11)

- Unreadable `/proc` or cgroup → fall back to `available_parallelism()`, then to 2.
  Never zero: `worker_threads(0)` panics the runtime at boot.
- A 1-core container → clamps to 1, not 2 (today it would oversubscribe).
- A host larger than the partition → clamps to 4, so a bigger box cannot silently
  widen the runtime past what the CPU isolation intends.
- f64→f64 mark path: values already flowing are unchanged bit-for-bit, because every
  current price round-tripped losslessly through f32 anyway.

## Failure Modes (Item 11)

- Derivation returns a wrong-but-plausible number → the boot log names the source and
  the resolved value, so it is readable rather than inferred.
- The env override still wins, so any surprise is one restart from reversed.
- Widening the price field cannot lose data: f64 is a superset of the f32 it replaces.

## Test Plan (Item 11)

- Unit tests for the CPU derivation: quota present, quota absent, unlimited, zero,
  oversized, and the clamp boundaries.
- Existing `order_runtime` mark tests continue to pass unchanged (they assert values,
  not the type's width).
- Full app-crate suite; `tv-guarantees` exit 0.

## Rollback (Item 11)

- 11a: set `TICKVAULT_TOKIO_WORKER_THREADS` in the systemd unit — config, no rebuild.
- 11b: revert the commit; no schema, no wire, no persisted format is involved.

## Observability (Item 11)

- 11a: the existing boot log line reports the resolved worker count and now also its
  source (cgroup / parallelism / fallback).
- 11b: no new metric. The existing `tv_mark_forward_dropped_total` is unchanged.

## Measured results (Item 11, recorded 2026-08-23 after implementation)

Evidence, not claims — every line below is a real number from this tree.

| What | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --no-deps -- -D warnings` (the CI gate) | clean |
| `cargo test -p tickvault-app` | 83 suites, **0 failures** (1,367 lib + integration) |
| `dhat_mark_forward_hot_arms_zero_allocation` | pass — the widened payload is still zero-alloc |
| `order_gate/mark_forward_disarmed` | **2.49 ns** (budget 100 ns) |
| `order_gate/mark_forward_armed_full` | **9.24 ns** (budget 100 ns) |
| `order_gate/mark_forward_armed_accept` | **69.9 ns** (budget 100 ns) |

**One test changed, and it is worth naming rather than burying.**
`test_mark_update_is_copy_and_small` pinned `size_of::<MarkUpdate>() <= 16` and FAILED:
`u64 + u8 + f64` pads to 24. That ratchet did its job — the widening has a real cost and
it refused to let it pass unnoticed. The bound moved to 24 only after the cost was
computed: the mark channel is bounded at `mark_channel_capacity` (default 8,192, config
max 65,536), so 8 extra bytes per slot is **+64 KiB at the default, +512 KiB at the
ceiling**, on a 32 GiB host. The contract the test exists to protect — a `Copy` payload
passed by value with no heap behind it — is intact; only the byte count moved.

**Deviation from the Test Plan above, stated plainly.** That plan predicted "existing
`order_runtime` mark tests continue to pass unchanged". One did not. The prediction was
wrong about a size assertion, and the record says so rather than being edited to match
the outcome.

**What Item 11 does NOT deliver (Rule 11).** 11a makes the runtime width follow the host
instead of a constant; it does not measure whether 2, 3 or 4 workers is the right width
for this workload — the first live session at scale is still that measurement, and the
env override is still how it gets acted on. 11b removes a latent precision class above
131,072; no price flowing today reaches it, so nothing observable changes.

---

## Item 12 — Close the gates that could pass while measuring nothing (2026-08-24 audit)

**Authorization.** Operator, 2026-08-24, verbatim: *"fix and resolve evrything dude okay?"* —
given in direct response to a message that enumerated the open findings of an 8-agent
workspace audit as a ranked table. Same authorization shape §28.2/§28.3 of
`daily-universe-scope-expansion-2026-05-27.md` already accept: a general go-ahead that
selects work the preceding message listed. Added to THIS plan rather than a new file
because the tree sits at exactly `PLAN_GATE_MAX_ACTIVE` (5) and a sixth `active-plan*.md`
would BLOCK every `crates/*/src/**.rs` push (V7) — the same slot constraint this plan's
own header records. Per `plan-enforcement.md` rule 5, items may be added to an approved
plan during implementation.

### Design

The audit's highest-value finding was not a bug in trading logic — it was a **gate that
reported success without measuring anything**, which is the false-OK class
`audit-findings` Rule 11 forbids and the class this repository has already retired twice.
Item 12 closes that class wherever the sweep found it, and adds the ratchets that stop it
returning.

- [x] **12a — `bench-gate.sh` fails closed on an empty comparison.** Two paths printed a
  success line and exited 0 while enforcing ZERO of the 11 absolute latency budgets: an
  empty Criterion tree, and benches whose ids had drifted away from their budget keys.
  Both now exit 3 with a named cause. Exit 3 is distinct from 1 (absolute breach) and 2
  (regression) so `bench.yml`'s baseline-ratchet condition (`gate_rc != '2'`) is
  unchanged. **Bite-proven in both directions** — against the pre-fix script the two new
  self-test cases return exit 0, against the fixed script exit 3, and all 7 pre-existing
  cases pass unchanged in both runs. The sibling `coverage-gate.sh` was fixed against this
  exact class on 2026-07-10 with a self-test; the fix had never been propagated.
  *(Landed 905155c.)*

- [ ] **12b — Arm the clippy gate on test targets, with the measurement CI asked for.**
  `ci.yml`'s clippy step carries a comment stating `--all-targets` is "deliberately
  omitted... its true warning count is UNMEASURED. Arming it is separate work with its own
  measurement". This item IS that work. Measured on this tree: 93 lints, all inside inline
  `#[cfg(test)]` modules — `cargo clippy --workspace --no-deps -- -D warnings` (what CI
  runs) is already clean, and the terminating error names `tickvault-app (lib test)`. Fix
  the test-target lints, add `--all-targets`, and extend
  `rust_only_guard.rs::ci_clippy_gate_stays_armed` to pin the new flag so it cannot be
  dropped silently.

- [x] **12c — Ratchet the dependency-graph interpreter class (scope hole EIGHT).**
  `rust_only_guard.rs` scans `Cargo.lock` for native BUILD systems
  (`KNOWN_NATIVE_BUILDERS`) and for nothing else. No test anywhere scans the graph for an
  embedded scripting runtime, so `rhai = "1"` in a crate manifest plus a script carried as
  an `include_str!` string would pass every existing guard green: no banned extension, no
  shebang, no spawn literal, no invocation token. Verified absent today (pyo3, mlua, rlua,
  rhai, deno_core, boa_engine, rquickjs, rustpython, v8, quickjs, duktape, wasmtime,
  wasmer, neon — none in the lockfile). This is the eighth time the guard has been found
  scoped to a surface rather than a question, which is the recurring shape its own §0.3
  records.

- [x] **12d — Back off the WAL no-segment retry arm.** `persist_record_resilient`'s
  `open_segment_resilient` failure path emits one `error!` per record and retries
  immediately. Its two siblings — the flush arm and the `write_record` arm — both sleep
  `WAL_WRITER_IO_RETRY_BACKOFF` first. On a full disk at the ~5,000 frame/s envelope that
  is ~5,000 open() syscalls and ~5,000 ERROR lines per second written to the disk that is
  already full. Disk-full DOES page (`WS-SPILL-01` has a live metric-filter alarm, checked
  in source), so this is amplification of a detected failure, not a silent one.

- [x] **12e — Drop the stale `crates/frontend-wasm` workspace exclusion.** The path does
  not exist and carries zero tracked files, so the entry excluded nothing while reading as
  a live carve-out to anyone auditing the Rust-only boundary. `cargo metadata` resolves
  identically without it.

### Edge Cases

Exit 3 must not collide with `bench.yml`'s baseline-save condition (`gate_rc != '2'`) — it
does not, and a run that measured nothing should still save whatever Criterion produced
rather than starve the cache (the deadlock the rc=1 comment already records). Arming
`--all-targets` must not break the calibration harnesses that legitimately print: the
`print_stdout` hit is handled at its site rather than by weakening the lint globally. The
lockfile scan must fail closed on an unreadable `Cargo.lock` rather than passing vacuously
— the exact defect 12a fixes elsewhere.

### Failure Modes

A future crate legitimately needing a sandboxed expression evaluator would trip 12c. That
is the intended behaviour: it forces a dated decision instead of an accident. The list is
a shrink-only allowlist in the established shape, so adding one is a visible diff.

### Test Plan

`bash scripts/bench-gate.selftest.sh` (9 cases, bite-proven both directions);
`cargo clippy --workspace --all-targets -- -D warnings` exits 0;
`cargo test -p tickvault-common --test rust_only_guard`;
`cargo test -p tickvault-storage`;
`cargo test --workspace` stays at 0 failures.

### Rollback

Every item is an independent revert. 12a/12d are localised to one function each; 12b is a
flag on one CI line plus test-module edits with no production reach; 12c is an additive
test; 12e is a manifest comment.

### Observability

12a and 12d change what an operator SEES on a failing run — a named cause instead of a
success line, and a legible error rate instead of a flood. No new metric, no new alarm, no
new `NotificationEvent`; the Dhan 4-item Telegram family is untouched.

#### 12b outcome — MEASURED, PARTIALLY DONE, arming DEFERRED (2026-08-24)

The measurement CI asked for is complete, and the auto-fixable half landed. The flag is
NOT armed, because arming it today would make Build & Verify fail on the first PR.

| | |
|---|---|
| Measured before any change | **93 lints, 102 error lines, exit 101**; all in inline `#[cfg(test)]` modules; the two hits resolving to production files were `note: the lint level is defined here`, i.e. the `#![deny(...)]` attribute lines |
| Fixed | 70 `inconsistent_digit_grouping` + needless borrows + `while let` on an iterator + `assert_eq!(x, true)` — all semantically neutral, verified by a **pure-reorder / identical-literal** diff check and a full suite re-run |
| Remaining | **13**, all pre-existing, none auto-fixable: 10 × `assertions_on_constants`, 1 × `field assignment outside of initializer`, 1 × `nonminimal_bool`, 1 × `unused mut` |
| Locations | `common/src/ingest_shed.rs` ×4, `common/src/constants.rs` ×3, `common/src/broker_order_events.rs` ×3, `common/src/config.rs` ×2, `common/tests/core_affinity_claim_guard.rs` ×1 |
| Fix shape | The 10 constant assertions want `const _: () = assert!(...)`, which this repo already uses (`constants.rs:3010`) and which is STRONGER — it moves the check from test-time to build-time. The `nonminimal_bool` one is `!(!a && b)`; simplifying it would make a deliberately-readable guard message harder to read, so it wants a justified `#[allow]`, not a rewrite. |

Arming is therefore one small, self-contained follow-up: fix 13, flip the flag, restore the
`--all-targets` assertion in `ci_clippy_gate_stays_armed`. Both were written and reverted in
this session rather than shipped red — a gate that fails on arrival teaches people to
bypass it.

#### 12f — What the LIVE box says (2026-08-24 session, read from AWS)

Source analysis cannot answer "is it working". These are live readings, and two of them
overturn conclusions the source audit reached.

| Reading | Value |
|---|---|
| Instance | `i-0c3fe906dad5492fc`, r8g.xlarge, ap-south-1b, stopped at time of read |
| Peak memory | **12.7% of 32 GiB ≈ 4.1 GB** — retires the long-standing UNMEASURED memory flag |
| Peak CPU | 46%, session average 13–23% |
| Free spill disk | 208.8 GB |
| WebSocket connections alive | **15** |
| Ticks dropped / WAL frames dropped / seals dropped | **0 / 0 / 0** |
| Candles sealed | **1,002,896** |
| Depth rows | 2,883,880 |
| Instruments never ticked | 0 (745 went silent later — normal for illiquid strikes) |
| **Ticks REFUSED by the aggregator** | **49,112**, and a 120-episode sample shows **100% timestamp refusals** — `refused_price = 0`, `refused_slot_exhausted = 0` |
| `AGGREGATOR-DROP-01` episodes | **713** in one session — roughly one every 30s, all day |

**The refusal finding is the important one and it is new.** `MIN_PLAUSIBLE_EXCHANGE_TS_SECS`
(1_600_000_000) refuses any tick whose exchange timestamp is below 2020-09-13, which is what
a never-traded instrument's `LTT = 0` sentinel looks like. So ~49k packets per session are
dropped before the fold and never become a candle.

The persistence layer disagrees with the aggregator about the same packet:
`row_timestamp_ist_nanos` deliberately falls back to the RECEIPT time for a sentinel LTT and
stores the row, while the aggregator refuses it outright. **The tick is kept; the candle is
not.** That is one packet, two policies, and it is a coherent explanation for OHLC gaps on
illiquid instruments — which is exactly the symptom the operator reported independently.

Also live-verified: **23 of the 76 EMF-declared metrics have never published a datapoint**,
and two alarms (`tv-prod-ticks-lost-spill`, `tv-prod-wal-frames-not-recovered`) sit on
metrics with no series — they read OK because nothing has ever arrived, not because
something was checked. `mem_used_percent` publishes under CWAgent with an `InstanceId`
dimension; `tv_process_rss_bytes` does not publish at all.

---

## ⚠ MERGE NOTE 2026-08-25 — TWO different sections were both numbered "Item 12"

`main` and the feed-hardening branch each appended an **Item 12** on
consecutive days, to different work. Both are kept, in date order, and
NEITHER is renumbered — the body text inside each block cross-references its
own item numbers, so renumbering the headings alone would leave every one of
those references pointing at the wrong section, which is worse than the
collision it would fix.

| Which | Dated | Subject | Where |
|---|---|---|---|
| **Item 12 (gates)** | 2026-08-24 | "Close the gates that could pass while measuring nothing" — 12b arming deferred, 12f live-box readings | the block ABOVE this note |
| **ITEM 12 (disk/O(1))** | 2026-08-25 | the ~25× write-amplification finding, the zero-timestamp false tick-loss page, and the O(1) track that continues into ITEMs 13–16 | the block BELOW this note |

**Reading rule:** inside each block, "Item 12" means THAT block's Item 12.
ITEMs 13, 14, 15 and 16 are unambiguous — they exist only in the lower block
and continue from ITEM 12 (disk/O(1)).

---
## ITEM 12 — DESIGN ADDENDUM (added 2026-08-25, operator: "yes fix and resolve evrythgi. see what the fuk is this bro when we have only 200 GB but how come this is comign around thsi much dude still i cant beleieve bro liek 4 tb how bro how can we resolve this dude?")

Two findings from the 2026-08-24 live-AWS sweep. Both are LIVE defects on the running
lane, not latent ones, which is what separates this item from Item 11.

### The measurement that opened it (Verified, live CloudWatch, session 2026-08-24)

| Reading | Value |
|---|---|
| `VolumeWriteBytes`, 03:00–12:00 UTC | **4,744 GB** |
| `VolumeReadBytes`, same window | 672 GB |
| Sustained throughput 09:30–15:30 IST | **495–500 MB/s** against a 500 MiB/s ceiling |
| Write latency, pre-open → mid-session | **0.86 ms → 2.30 ms** (2.7x) |
| `VolumeQueueLength` at market open | **7.3 avg**, 8.5 max |
| CPU over the same window | **13–27%** |

The disk is pinned at exactly its provisioned ceiling for the whole session while the CPU
idles. **This is not an O(1) problem and no hot-path work can fix it** — the decode path is
O(1), zero-alloc and DHAT-gated, and it is not the bottleneck. Recorded explicitly because
the operator asked for the O(1) guarantee in the same breath as the slowness, and the
honest answer is that the guarantee holds AND is irrelevant to this symptom.

### 12a — Commit cadence, not data volume, is producing the traffic

**Design.** `FLUSH_INTERVAL_MILLIS = 500` (`dhan_feed_stack.rs`) commits **7,200 times per
hour** into `PARTITION BY HOUR` tables that carry DEDUP UPSERT KEYS and grow to several GB
within the hour. QuestDB's per-commit rewrite cost scales with the **partition size**, not
with the batch size, so committing 10x more often costs ~10x the disk traffic for byte-identical
data. Measured real data is ~60-80 GB/session (depth 5.3 GB/hr measured 2026-08-18, ticks
~2.5 GB/day, plus candles) against 4,744 GB written — a ~60-80x amplification.

Raise `FLUSH_INTERVAL_MILLIS` 500 -> 5,000 and re-measure. Expected ~10x traffic reduction,
to ~470 GB/session (~20 MB/s average), which is far inside the existing 500 MiB/s ceiling.

**Why this cannot lose a tick.** Capture-at-receipt is the durability floor and it runs
BEFORE the fold and before any ILP flush (`dhan_feed_stack.rs`: "Capture-at-receipt is the
durability floor"). The WAL already holds the frame when the flush timer fires, so batching
changes only how long a row waits before becoming visible in QuestDB — never whether it was
captured. Per operator Quote 4 (2026-07-24, RAM-first absolute) no strategy, indicator or
risk path may read from the DB at all, so added DB visibility latency cannot reach a
trading decision by construction.

**Buffer bound.** questdb-rs enforces a 100 MiB `max_buf_size` and exceeding it wedges every
subsequent flush permanently (recorded at `seal_writer_task.rs` and `shadow_candle_writer.rs`).
At the measured ~3.6 MB/s of real depth rows a 5 s batch is ~18 MB, ~18% of the cap. The
implementation MUST assert the chosen interval against a measured worst-case row rate and
fail closed rather than approach the cap.

**HYPOTHESIS, not yet measured — stated as such.** The amplification MECHANISM above is
derived from the constants and the QuestDB commit model, not from a controlled experiment;
the box was stopped when this was written, so no live A/B was possible. The verification is
named and cheap: `VolumeWriteBytes` for the identical 03:00-12:00 window, one session before
and one session after the change, same universe size. If the traffic does not fall ~10x the
hypothesis is WRONG and the DEDUP-merge path is the next suspect, not the flush cadence.

### 12b — A zero timestamp is charged as tick loss and pages the operator

**Design.** `MultiTfAggregator::consume` refuses any tick whose `exchange_timestamp` falls
outside `[MIN_PLAUSIBLE_EXCHANGE_TS_SECS, MAX_PLAUSIBLE_EXCHANGE_TS_SECS]` (2020-09-13 ..
2050-01-01) and counts it as `refused_timestamp`, which feeds `AGGREGATOR-DROP-01` — a
PAGING code whose text reads "These ticks were NOT folded into any candle and NOT written."
Measured 2026-08-24: **17,931 refusals in one session**, 14,775 of them in the first hour,
then a steady ~400-580/hour; `refused_price` and `refused_slot_exhausted` were both ZERO.

The inconsistency is that the sibling case is already handled the opposite way: `p == 0.0`
is classified `untraded_sentinel` — a benign "this instrument has not traded" marker, not a
drop. A zero LTT is the same statement from the same packet, and `ws_lag_ms` already treats
a sub-plausible stamp as "a zero or garbage stamp" rather than corruption.

Split the single refusal into two: an explicit `ltt == 0` untraded sentinel (counted,
NOT charged as a drop, NOT paged) and a genuinely out-of-range stamp (kept as a hard
refusal, still paged, since that is the hostile-packet case the absolute bound was written
for — see the watermark-poisoning finding recorded at that constant).

**The gap that must be closed FIRST, before either branch is written.** The aggregator today
records only a COUNT, never the offending value, so nothing in the repository proves the
refused stamps are zero. That is an assumption and it is labelled one. Item 12b therefore
lands the observability half first — a throttled record of the actual refused value (min /
max / one sample, power-of-two throttled, zero-alloc) — and the classification half only
after a live session has SHOWN what the values are. Shipping the split on the assumption
would be exactly the false-OK this plan exists to prevent: if the stamps are not zero, a
benign-sentinel branch would silently swallow real corruption.

### What Item 12 deliberately does NOT do

- **No EBS throughput raise.** 500 -> 1000 MiB/s (+$20/mo) would hide the amplification
  rather than remove it, and 12a is expected to put traffic ~10x under the existing ceiling.
  It also requires a fresh dated operator quote under the daily-universe lock §7 Rule 3,
  which this quote is not: the operator asked how to RESOLVE the 4 TB, not to buy more pipe.
  If 12a's measurement shows the traffic still at the ceiling, the raise returns as its own
  quoted decision with the measured number behind it.
- **No DEDUP key change.** `capture_seq` in the `ticks` and `market_depth` keys is what makes
  WAL replay idempotent; removing it to cheapen the merge would trade a measured slowness for
  duplicate rows after any crash recovery. If 12a proves insufficient, the correct next step
  is a cheaper idempotence mechanism, designed on its own — never dropping the key.
- **No retention/window change.** `depth_hot_days = 1` and `intraday_hot_days = 1` are already
  at their floors and are not implicated: this is write TRAFFIC, not stored volume.

### ITEM 12 — CORRECTION (2026-08-25, same day, BEFORE any code was written)

Four parallel adversarial agents were run against 12a on the operator's instruction
("I dont need any hallucination or illusion... Try to attack evrythign"). **They killed
it.** The correction is recorded here rather than by editing 12a, per house convention,
because the ERROR is the durable lesson: 12a named a mechanism correctly and then blamed
the wrong constant, and had it shipped it would have changed nothing while being reported
as a fix.

**12a's named fix is WRONG (Verified, two agents independently).** `FLUSH_ROW_THRESHOLD
= 1_000` (`dhan_feed_stack.rs:3282`) is evaluated PER FRAME at `:2840`, BEFORE the timer
arm at `:2905`. The size and time triggers cross at 2,000 ticks/s; the envelope is
~5,000/s (12,500 at open). So the timer contributes ~0 flushes at open, and raising
`FLUSH_INTERVAL_MILLIS` 500 -> 5000 would move tick commits from ~5/s to ~5/s. The
constant's own doc comment already said so ("combined with the 500 ms time trigger it
caps depth at ~5 flushes/second") and 12a did not read it. **No flush-interval change is
authorized by this item.**

**The real data volume was also understated.** Inline 5-level depth (`DEPTH_KIND_5`) is
LIVE and unconditional at the production boot site (`dhan_feed_stack.rs:6332`,
`.with_inline_depth`), emitting up to 10 rows per Full-mode tick (`:3649`, `:3717`,
`:3735`) — ~50,000 d5 rows/s ON TOP of the measured 21,300 dedicated depth rows/s. At
72 B/row that is ~190 GB/session logical, not the 60-80 GB 12a assumed. True
amplification is therefore ~25x, not ~60-80x. Still large; smaller than claimed.

**Two surviving candidate causes, both O(partition rewrite), neither fixed by 12a:**

- **Candidate A — the candle path has NO batch valve at all.** `SEAL_DRAIN_INTERVAL_MS
  = 100` (`seal_writer_loop.rs:74`) is a bare timer; `drain_once`
  (`seal_writer_task.rs:122-235`) short-circuits only on an EMPTY ring and otherwise
  flushes unconditionally — there is no row-count threshold anywhere on this path, unlike
  ticks and depth. One flush carries rows for every timeframe table touched that cycle,
  and `tf_index.rs` enumerates **24** candle tables. Up to ~864,000 table-commits/hour.
  Worse, `candles_*` are `PARTITION BY DAY` (`shadow_persistence.rs:228`) while ticks and
  depth were deliberately given `PARTITION BY HOUR` for volume — so the highest-commit
  path in the system runs against partitions with 24x the merge surface.
- **Candidate B — `ticks.ts` is out-of-order by hours, structurally.**
  `tick_persistence.rs:428` stamps the row with `row_timestamp_ist_nanos(
  tick.exchange_timestamp, ..)` — the exchange LAST-TRADE time, not receipt. For a
  thin option at 11:00 whose last trade was 09:47, the row lands in an already-CLOSED
  hourly partition, forcing a reopen and merge-rewrite. `market_depth` stamps
  `received_at` (`:3685`) and is in-order. So on this reading depth is the VOLUME and
  ticks is the AMPLIFICATION.

**Uncounted writers on the same root volume** (none of which 12a accounted for):
QuestDB's own WAL (every byte at least twice), the `ws_frame_spill` raw-frame WAL
(~26 GB/session, `BufWriter`, no fsync), Loki (30-day retention), `errors.jsonl`, and the
`tv-questdb-data` docker named volume which lives on root.

**NOT ESTABLISHED and not to be asserted:** QuestDB 9.3.5's exact WAL+DEDUP apply cost.
No agent could source it from this repository and none invented it. Every statement above
about rewrite cost scaling with partition size is INFERRED from the columnar-rewrite model
and is labelled as such.

### ITEM 12 — THE THREE DECISIVE EXPERIMENTS (all require the box; none can run from a dev container)

Ordered cheapest-first. Each is one measurement that discriminates between live candidates.
No code fix may be proposed under Item 12 until at least E1 or E2 has returned.

- **E1 (settles Candidate B, zero risk, read-only):**
  `SELECT count() FROM ticks WHERE ts < dateadd('h', -1, received_at)`. A non-trivial
  count proves ticks are landing in closed partitions and the designated timestamp — not
  any flush constant — is the amplifier.
- **E2 (settles Candidate A, one constant, one session):** raise `SEAL_DRAIN_INTERVAL_MS`
  100 -> 2000 for a single session and compare EBS `VolumeWriteBytes` over the identical
  03:00-12:00 UTC window. Commit count falls 20x; row count is unchanged. If writes fall
  roughly proportionally, commit amplification is confirmed and the fix is a row-count
  valve plus HOUR partitioning on `candles_*`. If writes barely move, the candle path is
  exonerated.
- **E3 (settles 12b, read-only, minutes):** the raw frames of every refused tick are
  already on disk — capture-at-receipt WALs the frame BEFORE parse
  (`dhan_feed_stack.rs:1149`, `ws_frame_spill`, `data/spill/`). Decode one segment and
  read the actual LTT values and their `security_id`s. This ends the guessing entirely.

### ITEM 12b — SUPERSEDING FINDINGS (the refusal is worse than 12b described)

- **Most likely cause: `IDX_I` index packets, newly arriving.** `IDX_I_FEED_MODE =
  FeedMode::Quote` (`connection.rs:506`) landed 2026-08-21; the same file records that
  BEFORE that flip indices produced ZERO packets and `never_ticked` equalled the index
  count exactly. The measurement is 2026-08-24 — three days later. An index has no trades,
  so "last TRADE time" is meaningless for it while LTP is a real number: precisely the
  price-sane / timestamp-insane shape that reaches this gate. **Unknown:** nothing in the
  repo states Dhan's LTT for `IDX_I`, and `docs/dhan-support/2026-05-18-idx-i-quote-full-mode-support.md`
  asked Dhan exactly this and NO ANSWER IS RECORDED. 119 indices also do not obviously
  produce 17,931 refusals, so the cause may be only partly this.
- **A refused instrument is INVISIBLE, not merely undercounted.** The gap detector
  observes at `:1188`, BEFORE `consume_tick` at `:1203`, unconditionally. So an instrument
  whose every tick is refused gets no candle and no `ticks` row, and is simultaneously NOT
  silent to `scan_silence` / RISK-GAP-03 — it reads as healthy. It is indistinguishable
  from an instrument that simply never traded. This is a false-OK of the exact class the
  charter forbids and it is LIVE today.
- **The `reason` label never reaches CloudWatch.** EMF dimensions are `[["host"]]`
  (`cloudwatch-agent.json:23`), so `tv_aggregator_tick_refused_total{reason}` is summed
  across reasons and the operator cannot separate a timestamp refusal from a price or
  slot-exhaustion refusal at all.
- **The page the operator receives describes a different failure.** The alarm firing on
  `AGGREGATOR-DROP-01` (`error-code-alarms.tf:244`) carries a description about
  SEAL-drop, not tick refusal — so the runbook text does not match the event.
- **A legitimately tradeable tick can be refused.** The order is
  `price -> untraded_sentinel(p==0.0) -> timestamp -> session`, so a tick with a REAL
  price and a bad stamp is refused whole and never reaches the sentinel escape hatch.

### ITEM 12b — SECURITY REVIEW RESULT (2026-08-25): the sentinel must NOT be write-through

An adversarial security pass found a defect in 12b as drafted. Recorded before any code.

**The trap.** 12b reasoned by analogy to `untraded_sentinel` (`p == 0.0`). That analogy is
WRONG in the one way that matters. Verified at `dhan_feed_stack.rs:1305-1367`: a
`hard_refusal` (today's `refused_timestamp`) returns at `:1333` BEFORE
`append_tick_with_seq` at `:1337`, so no row is written; a `candle_only_refusal` (which is
where `untraded_sentinel` sits) DOES write the tick row, stamped
`ts = exchange_timestamp * 1e9`.

So routing an `ltt == 0` sentinel the same way as the price sentinel would persist rows
with **`ts = 0`, i.e. 1970-01-01**. For the price sentinel the corrupt field is the VALUE
and the timestamp is sound; here the corrupt field IS the timestamp. The file's own comment
(`:1296-1304`) already calls writing a garbage designated timestamp "worse than losing it".

**And it would be unbounded.** Every `ltt == 0` packet carries a distinct `capture_seq`, so
the DEDUP key never collapses them: a corrupt or hostile zero-LTT stream would grow a single
1970 partition without limit — while being UNPAGED, because the whole point of the change is
to stop it paging. That is a strictly worse outcome than the current behaviour, arrived at
while trying to fix it.

**Binding correction.** The `ltt == 0` case must become a FOURTH category: counted, benign,
NOT paged, and still **row-refused** — never a copy of `untraded_sentinel`'s write-through
path. A PR that routes it write-through is a REJECT.

**Other findings from the same pass (all Verified):**
- `multi_tf_aggregator.rs` is OUTSIDE the frozen area (`FROZEN_DIRS` = `indicator/`,
  `strategy/` only, per `operator_boundary_indicator_strategy_guard.rs:36-37,49`). **No §28
  lift is required** for Item 12 — good, because a §28 lift would need its own dated quote.
- No attack path to the watermark IF and ONLY IF the split is scoped to `ltt == 0` exactly:
  `0` can never exceed `watermark_secs`, and the ~2106 poisoning vector lives at the MAX
  bound, which stays untouched. Any widening of the MIN bound instead of an exact-zero case
  WOULD reopen it.
- `test_watermark_cannot_be_poisoned_by_an_all_ones_timestamp`
  (`multi_tf_aggregator.rs:1368-1402`) asserts `ts == 0` must be `refused_timestamp`. It
  must be EDITED, never gutted: `0xFFFFFFFF`, `MAX+1` and `1` must all remain refused.
- `ConsumeStats::folded()` (`:173-190`) must gain the new field — required for correctness,
  not a weakening.
- The observability half must EXTEND the existing throttled 30s delta report
  (`dhan_feed_stack.rs:2977-3009`). It must NOT add a per-tick log line (flood at ~18k
  events/session) and MUST NOT add a per-instrument metric label (the cardinality ban:
  per-instrument CloudWatch series were priced at ~$1,369/mo against a budget whose
  automatic action stops the trading box).
- Change (A), the batching/commit-rate work, is LOW risk: capture-at-receipt precedes the
  fold and flush, and flush failure is DEDUP-idempotent via spill + WAL replay. Larger
  batches widen only the VISIBILITY-latency window, never the loss window. No secret flows
  through an ILP buffer.

## ITEM 13 — THE O(1) DEFECT, AND WHAT MONEY CANNOT BUY (2026-08-25, operator: "i just need always O(1) dude i dotn need any slowness ddue irrespective of any situaitons" + "even if we need to reach the max 150 usd also let su gfo ahead dude but ensure to achieve alwyas O(1)")

Five parallel adversarial agents were run on the operator's instruction to attack
everything. Budget authorization to $150/mo is recorded as Quote 19 in
`daily-universe-scope-expansion-2026-05-27.md`. **It is deliberately unspent — see 13d.**

### 13a — THE defect: the ILP flush runs ON the drain task (Verified, CRITICAL)

`flush_and_record` (`dhan_feed_stack.rs:2564-2578`) is a **synchronous blocking
ILP-over-HTTP round trip** executed inside `block_in_place` (`:2524`) **on the frame-drain
task** — the same `tokio::select!` loop (`:2705`) that carries frame ingest, the 5 s
catch-up seal and the 30 s silence scan.

So while a flush is in flight, **no frames are drained**. That is the causal chain from a
saturated disk to LOST TICKS, and it is the reason the operator sees "slowness" that no
amount of hot-path O(1) work can remove:

    disk saturated -> flush RTT grows -> drain stalls -> ring/socket buffer fills
      -> Dhan skips a slow consumer forward to "latest available state"
      -> intermediate ticks are dropped VENDOR-SIDE, with no sequence number to detect it

**This is the single highest-value fix in this plan and it costs nothing.** Decoupling the
flush from the drain (own task + bounded channel) removes the coupling entirely. Until it
is decoupled, no O(1) guarantee can honestly be given for the END-TO-END path, however
O(1) the decode is.

### 13b — Flush RATE is the axis, and the tick threshold was never re-sized (Verified, HIGH)

`FLUSH_ROW_THRESHOLD = 1_000` (`:3282`) is evaluated per frame. Measured emission is
~1.44 rows/sec per continuously-ticking instrument (derived in 13c), so:

| Instruments | rows/sec | flushes/sec @1,000 | drain blocked @5 ms RTT |
|---|---|---|---|
| 8,315 (today) | ~11,900 | ~12 | ~6% |
| 24,600 (authorized) | ~35,300 | ~35 | ~18% |

`DEPTH_FLUSH_ROW_THRESHOLD` was raised 10x to 10,000 for exactly this reason, and its own
comment says so verbatim: *"Payload is the wrong axis here; FLUSH RATE is"* (`:3285-3306`).
The TICK threshold sat at 1,000 through that reasoning and through the 21 -> 24 TF append.
Raising it is the cheap mitigation; 13a is the real fix.

### 13c — A hostile finding, CORRECTED before it was acted on (Verified)

An agent reported the row rate as **3.39/sec/instrument** (28,200 today, 83,400 at target,
42% drain blocked), computed as the harmonic sum over all sixteen second-scale frames
S1..S15 + S30. **That is wrong by 2.4x**, and acting on it would have justified far more
drastic surgery than the system needs.

`TfIndex::is_operator_requested()` (`tf_index.rs:388`) gates ROW EMISSION to the operator's
**thirteen** frames (S1 S5 S10 S15 S30 + M1 M2 M3 M5 M15 M30 M60 + D1), with three live
production call sites (`dhan_feed_stack.rs:1227`, `:1424`, `:1679`). The eleven unrequested
frames (S2 S3 S4 S6 S7 S8 S9 S11 S12 S13 S14) fold but never emit. True rate is
1 + 0.2 + 0.1 + 0.0667 + 0.0333 + minute frames = **~1.44 rows/sec**.

**And this closes off a comfortable answer.** That gate has been live since 2026-08-18
(#1768), so it was ALREADY ACTIVE during the 2026-08-24 session that wrote 4,744 GB. The
row rate therefore does NOT explain the amplification, and no further timeframe trimming
will. Recorded because the agent's number was plausible, alarming, and would have sent the
next session cutting capability the operator explicitly paid for.

### 13d — Why the $150 authorization is NOT being spent yet

Quote 19 authorizes up to $150/mo (live: $48.87 actual, $61.51 forecast, $130 limit — the
widest margin this account has had). The EBS raise 500 -> 1000 MiB/s is +$20/mo and fits.
It is held anyway, on three grounds:

1. **It does not deliver the thing asked for.** The requirement is O(1) with no slowness.
   13a is a *coupling* defect: a faster disk shortens the stall, it does not remove the
   drain from the flush's critical path. The $0 fix is the one that gives the guarantee.
2. **~25x amplification is still unexplained** (13c removed the row-count explanation).
   Doubling throughput against a 25x inefficiency buys one doubling and leaves the defect;
   at 24,600 instruments the same wall returns.
3. **The newest prime suspect is a config value, not a capacity one:**
   `QDB_CAIRO_WRITER_DATA_APPEND_PAGE_SIZE = 16777216` (16 MiB) in
   `deploy/docker/docker-compose.yml`, across 24 tables x ~17 columns. If page granularity
   is the amplifier the fix is an env var at $0. Also newly noted from the same file:
   `QDB_CAIRO_O3_MAX_LAG = 60000000` (60 s) — a tick whose last-trade time is 90 minutes
   stale falls FAR outside that window and forces a hard partition merge, which strengthens
   the out-of-order candidate rather than the commit-rate one.

The raise is taken the moment a measurement shows traffic still at the ceiling after the
amplification fix — with a number behind it, not a guess.

### 13e — Scale cliffs found by the same pass (recorded, not yet fixed)

- **HIGH — ring-full drops are unrecoverable within the session.** `pool_supervisor.rs:127-160`:
  a full ring counts and DROPS the frame. It is in the WAL, but `dhan_feed_stack.rs:1007-1021`
  states the re-fold path needs a LIVE ring, so recovery is *"the next boot re-folds them"*.
  A mid-session backpressure episode is therefore silent tick loss until tomorrow.
- **HIGH — the measured `catch_up_seal_all` cost is the EMPTY case.** `multi_tf_aggregator.rs:1676-1679`
  measures "none seals"; the 9.67 ms figure is pure traversal. At a minute boundary with
  24,600 slots the sweep must EMIT, and each emission runs the absorption chain. The
  recorded number does not bound the real one. Unknown.
- **MEDIUM — subscribe dispatch is unpaced.** 24,600/100 = 246 messages sent back-to-back
  (`connection.rs:794-850`, no delay). Dhan's subscribe rate limit is Unknown; a throttled
  subscribe produces no error, only absence — and the 09:12 readiness deadline would miss
  silently.
- **LOW — memory is fine.** `AggregatorCell` 6.2 KB x 24,600 = 153 MB; seal ring 86 MB;
  all per-instrument maps cap fail-closed at 25,000. 32 GiB holds it comfortably.
- **No locks on the drain hot path** — single-owner `&mut`, verified.

### 13f — The honest O(1) verdict (audited stage by stage)

**Per PACKET the path IS O(1) and effectively zero-allocation, and it is PROVEN, not
claimed:** fixed-offset `from_le_bytes` decode, one O(1)-average composite-key lookup, 24
scalar folds, one bounded `try_send`, one ILP append — with `dhat_allocation.rs`,
`dhat_multi_tf_fold.rs` (exactly 0 allocations over 10,000 folds), `dhat_ws_lag.rs` and
`dhat_ws_reader_zero_alloc.rs` gating every PR. No banned pattern appears on the per-tick
path.

**Three stages break strict O(1) and must never be described otherwise:**
1. **The ILP flush** — O(rows) AND blocking AND on the drain task (13a). The real defect.
2. **Slot allocation** on an instrument's first tick — a `Vec` growth step is O(n) and
   unbounded; mitigated by boot pre-sizing, not eliminated.
3. **Seal-refusal escalation** (`:4192`) — writes to disk INLINE on the tick thread when
   the seal channel is full.

**Per FRAME it is not O(1)** either: `drain_main_feed_frame` walks every stacked packet,
bounded only by `MAX_PACKETS_PER_FRAME = 70,000`.

**Correction to CLAUDE.md, found by this audit:** the codebase map credits this path with a
`papaya` concurrent-map lookup. There is **zero `papaya` on the live tick path** —
`multi_tf_aggregator.rs:348` is a plain `HashMap<CompositeKey, u32>` into a dense `Vec`
index. Still O(1) average; the claim was wrong about the type, exactly as the
`instrument_registry` row in that same table was wrong in 2026-08-07.

## ITEM 14 — DECOUPLING THE FLUSH: design, conditions, and why it must NOT ship first (2026-08-25)

Item 13a named the flush-on-drain coupling as THE O(1) defect. A design pass and a hostile
pass were run in parallel on the fix. They agree on the conditions and disagree on the
verdict, and the disagreement is the useful part: **the hostile pass found an own-goal that
means this fix must not land before the amplification is diagnosed.**

### 14a — What the design pass settled (Verified)

Reuse the proven seal-writer shape (`seal_writer_runner.rs` / `_loop.rs` / `_task.rs`), do
not invent one:

| Element | Decision |
|---|---|
| What crosses the boundary | an already-built ILP `Buffer`, **one send per FLUSH**, never per tick — so the per-tick DHAT zero-alloc guarantee is untouched |
| Channel | bounded `mpsc<TickBatch>`, capacity **DERIVED** (`TICK_SPILL_MAX_BYTES / max_batch_bytes`), never a literal — the `SEAL_MPSC_CAPACITY` lesson, where a 200k literal silently force-dropped 400k seals nightly |
| Buffer reuse | writer returns emptied buffers on a second bounded channel; steady state allocates nothing |
| Full-channel policy | **SPILL**, reusing the tick spill that ALREADY exists (`tick_persistence.rs:636` `spill_failed_ilp` writes `Buffer::as_bytes()` verbatim, 512 MiB cap at `:606`, replayed by `tick_spill_replay.rs:359`). Blocking reinstates the defect; dropping violates never-lose-a-tick |
| Shutdown | preserve the `:2883-2906` order, then drop the sender as EOS and `await` the writer under `timeout(2 x request_timeout)` = 10 s so the 17:30 stop cannot hang; the writer's final act is to spill its tail |
| Placement | `crates/storage/src/tick_writer_runner.rs` + `tick_writer_loop.rs`; wiring in `dhan_feed_stack.rs` |

**`capture_seq` is already safe and needs no change** (Verified, and this was the biggest
worry): it is derived from the WAL frame sequence (`ws_frame_spill::packet_capture_seq` ->
`capture_seq_from_frame_seq`, `dhan_feed_stack.rs:1149,1172,1337`), NOT minted at flush. So
it is replay-stable and wholly independent of when the flush happens. One writer + one FIFO
channel preserves batch order, and because the DEDUP key carries `capture_seq` a spilled
batch replayed later collapses idempotently instead of duplicating.

### 14b — The hostile pass: two corrections and one own-goal

1. **The premise was half right, and the code's own comment is stale self-justification.**
   `block_in_place` migrates OTHER tasks off the worker and spins a replacement, so the WS
   read loops are NOT stalled today — only the drain loop itself is. The comment at `:2500`
   claiming "on a 2-worker host that is HALF the runtime" overstates it. The real defect is
   **unbounded REPETITION** of 5 s-timeout flushes on a 500 ms timer (~100% occupancy), not
   any single flush. One stall is survivable: the ring holds 65,536 frames, ~13 s at 5,000
   fps.
2. **Today's blocking flush IS backpressure.** It is ugly, but it self-throttles and nothing
   downstream is silently dropped. Remove it without a correct overflow policy and the drain
   runs free while the writer falls behind — and the seal analogue this design mirrors,
   `escalate_refused_seal` (`:4192`), is documented verbatim as *"a synchronous disk append
   on the fold path"*. Escalating to a synchronous append **on the same saturated EBS volume
   that caused the backlog** would swap one blocking path for another and add a queue. That
   is the shell game, and it is avoided only because ticks already have a spill tier.
3. **THE OWN-GOAL (HIGH, Assumed, unmeasured):** a decoupled writer batches more
   aggressively, so each commit spans a WIDER `ts` range, which **increases O3 merge work**
   — the very write amplification Item 12 is trying to remove. Nothing in the repo measures
   this.

### 14c — Therefore: Item 14 does NOT ship before Item 12's measurement

This is the binding sequencing decision of this plan, and it is deliberate:

**The amplification cause must be identified FIRST.** If the amplifier turns out to be
commit WIDTH (out-of-order `ts` into closed partitions, strengthened by
`QDB_CAIRO_O3_MAX_LAG = 60 s`), then wider batches make it worse and the decoupling must
ship WITH a batch-width cap. If the amplifier is page granularity
(`QDB_CAIRO_WRITER_DATA_APPEND_PAGE_SIZE = 16 MiB`) or commit COUNT, wider batches help.
**The same change is beneficial or harmful depending on a fact nobody has measured.**
Shipping it blind against a saturated disk is the gamble the operator explicitly forbade.

### 14d — Conditions, binding on the implementing PR

- **C1** The overflow policy is SPILL to the existing tick spill tier — never a silent drop,
  never a blocking append introduced on the fold path.
- **C2** The drain ships fully-formed, pre-stamped rows; the writer NEVER mints
  `capture_seq`. (Already true — must not regress.)
- **C3** `feed_health.record_ticks` and `LAST_TICK_AGE_GAUGE` move to the **writer**, not
  the drain. **This is rule-gated, not stylistic:** `dhan-rest-only-noise-lock` §2.3b-i
  chose the age GAUGE over the counter deliberately, and `no_ticks_alarm_gauge_guard.rs`
  pins it. Stamping at hand-off would silently redefine the only dead-lane alarm from
  "rows persisted" to "rows decoded" — forging liveness for rows a crash would take. The
  guard re-bless and a dated §2.3b edit land in the SAME PR.
- **C4** Shutdown awaits writer completion under a bounded grace; batch width is capped at
  today's 500 ms span until 14b(3) is measured; `max_buf_size` headroom is const-asserted.
- **C5** The questdb-rs `max_buf_size` = 100 MiB wedge (`seal_writer_task.rs:229`,
  `shadow_candle_writer.rs:359`) is reached in ~28 s of stall at the 24,600-instrument rate,
  and decoupling makes it EASIER to reach because nothing throttles the producer. A
  size-based cut must fire well before it.

### 14e — An infrastructure blocker on the observability half (Verified)

The design calls for `tv_dhan_tick_writer_queue_depth` / `_high_water` / `_full_total` /
`_flush_seconds`. **No new EMF metric name can ship**: `user-data.sh.tftpl` renders at
exactly its 15,872-byte budget with **ZERO free** (measured, `dhan-rest-only-noise-lock`
§2.3d-ii). Until the boot-path restructure that section describes, backpressure visibility
must go via the metric-filter/log route (the §2.3d-i precedent), or the queue grows unseen
— which for a backpressure queue is the worst possible blind spot.

## ITEM 15 — MEASURED HOST TELEMETRY, and two corrections to my own earlier claims (2026-08-25)

Operator challenged the depth of the audit: *"how did you check the entire memory as how much GB
it is used and did you query from db and did you start the instance"*. He was right — memory had
NOT been measured. It has now. Two things I previously reported are **WRONG** and are corrected
here rather than quietly amended.

### 15a — CORRECTION: the CPU is NOT idle. I read the wrong meter.

Items 12-14 repeatedly state "CPU idles at 13-27%" and build the argument
"this is I/O-bound, not CPU-bound" on it. That number is `AWS/EC2 CPUUtilization` — the
HYPERVISOR view averaged across all 4 vCPUs. The in-guest CloudWatch agent, same window,
same 300 s period, `CWAgent cpu_usage_active{cpu=cpu-total}`:

| 2026-08-24 UTC | AWS/EC2 (hypervisor) | CWAgent (in-guest) |
|---|---|---|
| 04:00 | 40.9% | **66.7%** |
| 04:30 | 23.4% | **67.7%** |
| 04:45 | 19.9% | **69.7%** |
| 04:55 | 21.1% | **67.9%** |

The app and QuestDB are confined to a **2-core cpuset of 4 vCPUs** (`docker-compose.yml`
`cpuset: "2,3"`, plus the systemd confinement), so the hypervisor average roughly halves the
number the workload actually experiences. **Real sustained CPU is ~67%, not 13-27%.**

This does not overturn the disk finding — the volume is still pinned at its 500 MiB/s ceiling —
but it DOES overturn the framing. The box is near-saturated on **both** CPU and disk, and any
claim of the form "there is CPU headroom, so the fix is X" must be re-derived. Recorded loudly
because this repository's own O(1) table has twice recorded a stale number manufacturing a false
finding, and this is the same class committed by me, in this plan, three times.

### 15b — THE FINDING NOBODY HAD MEASURED: the disk reaches 80% every session

`CWAgent disk_used_percent{path=/, device=nvme0n1p1, fstype=xfs}`, 2026-08-24:

| IST | Used | % of 200 GB |
|---|---|---|
| 08:30 (boot) | 9 GB | 4.5% |
| 11:30 | 82 GB | 41.1% |
| 14:00 | 136 GB | 68.0% |
| **15:00** | **160 GB** | **80.1%** |
| 16:00 (post-retention) | 146 GB | 72.8% |
| 20:00 | 160 GB | 79.9% |

**~151 GB consumed in ONE nine-hour session**, with `depth_hot_days = 1` and
`intraday_hot_days = 1` already at their FLOOR — there is no retention lever left to pull.

At the authorized 24,600 instruments (~3x today's 8,315) this projects to **~450 GB against a
200 GB volume**, i.e. **ENOSPC mid-session**. That is not a capacity inconvenience: on ENOSPC the
`ws_frame_spill` append fails, `WalRingSink::accept` returns `WalDropped`, and **the frame is
gone BEFORE the ring and BEFORE parse** — the durable floor is the first thing a full disk
removes. Unrecoverable tick loss, which is precisely what the whole architecture exists to
prevent.

This also cross-checks the amplification: 4,744 GB written against ~151 GB retained is **~31x**,
consistent with the ~25x derived independently in Item 13.

### 15c — Memory is genuinely fine, and that is the answer to the operator's question

| Measure | Value | Source |
|---|---|---|
| Host memory used | **3.00-4.07 GiB of 32 GiB (9.4-12.7%)** | `CWAgent mem_used_percent{InstanceId}` |
| App process RSS | **0.31 GB** flat all session | `Tickvault/Prod tv_process_rss_bytes` |
| Projected live structures at 24,600 | ~527 MB | space audit, Item 15e |

Memory is NOT a constraint and is not close to being one. The r8g.xlarge's 32 GiB was bought for
the 13-timeframe requirement and is barely touched.

**Method note worth keeping:** the first query returned NOTHING and looked like "the CW agent
publishes no host metrics" — a false finding I nearly reported. CloudWatch
`get-metric-statistics` requires an EXACT dimension match; querying without `--dimensions` looks
for a zero-dimension metric that does not exist. The metrics were there all along.

### 15d — Observability: the operator cannot see 80% of what the system measures

| Measure | Count | Source |
|---|---|---|
| `tv_*` metric names with a production producer | **373** | source scan |
| Present in the CloudWatch EMF selector | **76** | `cloudwatch-agent.json` + `user-data.sh.tftpl`, byte-identical (lockstep holds) |
| **Never reach the operator** | **~297 (80%)** | difference |

Invisible today: universe health (`tv_dhan_universe_*`, `tv_dhan_live_universe_instruments`), the
15:31 cross-verify outcome, indicator poisoning/slot exhaustion, order-budget refusals, load-shed
transitions, seal rescue pressure. All are on the box's own `:9090/metrics` and nowhere else —
and no new EMF name can ship until the `user-data.sh.tftpl` zero-byte-budget restructure.

**EIGHT permanently-green dead monitors** — alarms or dashboard widgets whose metric has NO
producer anywhere in `crates/*/src/`: `tv_order_fill_lag_seconds`, `tv_orders_placed_delta_total`,
`tv_seal_writer_drain_dropped_total`, `tv_dlq_ticks_total`, `tv_spill_dropped_total`,
`tv_websocket_pool_all_dead`, `tv_websocket_failed_connections_count`,
`tv_aggregator_seals_emitted_total` (the live name is `tv_dhan_feed_seals_emitted_total` — a
rename that orphaned its alarm). Every one reads as health.

**~29 selected metrics carry a label whose distinction EMF destroys** (dimensions are
`[["host"]]`, so labels are summed away). `tv_dhan_feed_drain_frames_total` merges TEN outcomes —
folded, unparseable, write_failed, depth_unconsumed, truncated — into one number. The alarm fires
and can never say why.

### 15e — Two more space/storage findings from the same pass

- **The SIXTH omission in the CLAUDE.md non-O(1) table: `oms/engine.rs:191 order_no_aliases`.**
  `HashMap<String, String>`, `with_capacity(64)` (a PRE-SIZE, not a bound), inserted per broker
  `order_no` on every re-index, cleared only by `reset_daily`. `MAX_TRACKED_ORDERS = 25_000`
  counts `self.orders.len()` ONLY and never consults the alias map. **It was created by the very
  2026-08-22 repair that added that cap**, and the table's row lists only
  `{orders, super_orders, verify_states}`. Live in paper mode. Sixth time, same pattern: the
  newest per-entity map is the one with no row.
- **Depth has NO flush-failure rescue tier.** `ticks` gained a spill rescue after losing 1,377
  ticks on 2026-08-21; `market_depth` did not — its own comment says *"These levels are gone from
  the table"* (`depth_persistence.rs:584`). At 250 x 40 rows/s a single ILP timeout drops ~10k
  rows. The raw frames survive in the WAL, but **nothing re-folds depth from them**.
- **`candles_<tf>` DEDUP is `ts, security_id, segment, feed` with NO source discriminator**
  (`shadow_persistence.rs:114`). Two writers stamping `feed='dhan'` silently upsert over each
  other; this ALREADY happened (live lane vs `rest_candle_fold`), and the only thing preventing
  it today is `enabled = false` in config — a config accident, not a key.

### 15f — Still NOT done, stated plainly

The instance has NOT been started and the database has NOT been queried. Every number above is
CloudWatch or source. The DB questions — rows per table, whether `ticks.ts` is out of order,
whether the refused ticks are the indices — remain open and are answered in one command by
`scripts/diagnose-write-amplification.sh` once the box is up (08:30 IST, or on operator request).

---

## Item 16 — the cross-verify was blind to the last 10 minutes of every session (FIXED)

- [x] **16a — `SESSION_CLOSE_SECS_OF_DAY_IST` derived from the canonical constant, not restated**
  - Files: `crates/app/src/dhan_live_crossverify.rs`
  - Tests: `secs_of_day_and_is_in_session_boundaries_are_half_open`,
    `deterministic_run_ts_nanos_is_one_minute_past_the_close_regardless_of_fire_time`
- [x] **16b — the fire time moved in lockstep with the window end**
  - Files: `crates/app/src/dhan_feed_stack.rs`
  - Tests: `test_crossverify_schedule_lands_on_1531_ist_and_never_double_fires`,
    `test_crossverify_day_origin_covers_the_entire_session_not_just_the_first_45_minutes`

### What was wrong

`dhan_live_crossverify.rs` carried its OWN `SESSION_CLOSE_SECS_OF_DAY_IST = 15*3600 + 30*60`
— a **private duplicate** of the session end. On 2026-08-07 NSE added the 15:30–15:40 closing
session and six production sites moved `55_800 → 56_400` with a dated comment
(`constants.rs`, `rest_candle_fold.rs`, `tf_consistency_boot.rs`, `feed_scoreboard_boot.rs`,
`trading_pipeline.rs`, `day_ohlc_orchestrator.rs`). This file kept its own copy and drifted for
**eighteen days**.

### What it cost — and what it did NOT cost

It did **not** produce false findings, and that is why nobody saw it. `is_in_session` gated
BOTH sides of the join, so the window stayed symmetric and every verdict it printed was
honest. What it produced was a **blind spot**: ten minutes of every session — specifically
the closing-auction window the NSE migration exists for — were structurally unverifiable by
the one check `websocket-connection-scope-lock.md` calls *"the ONLY ground truth the revived
Dhan feed has"*.

It also mis-aimed the tail amnesty. `is_tail_minute` derives from this constant, so it
excused 15:28–15:29 while the genuinely-unsealed tail had moved to 15:38–15:39 — meaning the
two minutes that legitimately are unsealed at run time were being counted as **real loss**,
and two minutes that were fine were being excused.

### Why the two halves had to move together

The window end (`dhan_live_crossverify.rs`) and the fire time
(`dhan_feed_stack::XVERIFY_RUN_AT_SECS_OF_DAY_IST`) are **coupled**. Widening the window to
15:40 while still firing at 15:31 would have turned a silent blind spot into a flood of false
`MissingLive` findings — comparing nine minutes that have not happened yet. Both moved in one
change: close 15:30 → 15:40, fire 15:31 → 15:41.

### The guarantee that this cannot drift a seventh time

Three `const _: () = assert!(...)` lockstep guards — the close tracks
`TICK_PERSIST_END_SECS_OF_DAY_IST`, the window is non-empty, and the run stamp is strictly
after the close. **Bite-proven 2026-08-25:** restoring the `15 * 3600 + 30 * 60` literal fails
the build with `error[E0080]: evaluation panicked: the cross-verify session close must track
the canonical persistence end`. The four ratchets that pinned the old values were re-blessed
to the corrected ones **and rewritten to DERIVE** — `375` is now
`(close − open) / 60` and the tail minutes are `close − 60` / `close − 120`, so the next
session-hours change moves them automatically instead of failing four tests.

### Honest envelope

This restores the ability to VERIFY the CAS window. It does **not** prove the feed captures it
— that is what a non-zero `compared` covering 15:30–15:39 will show on the first session after
this lands, and nothing before then. Defects #1 (the 200,000-row live-side truncation), #3
(every target labelled `instrument: "INDEX"`), #4 (no `[dhan_live_crossverify]` config section)
and #5 (silent inline-depth drops) are untouched by this item and remain open.

---

## Item 17 — three live defects found by parallel adversarial review (FIXED)

- [x] **17a — `append_inline_depth` dropped depth rows in silence, four ways**
  - Files: `crates/app/src/dhan_feed_stack.rs`
  - Tests: `every_inline_depth_drop_is_counted_never_silent` (bite-proven)
- [x] **17b — the paper book's sid ceiling manufactured a permanent false divergence**
  - Files: `crates/app/src/order_runtime.rs`
  - Tests: `a_full_tripwire_never_manufactures_a_mirror_divergence` (bite-proven)
- [x] **17c — the comparator's arming log told the operator a fire time it no longer uses**
  - Files: `crates/app/src/dhan_feed_stack.rs`

### 17a — depth loss with no counter and no log

The dedicated depth drain counts every refusal. The INLINE d5 twin, written four days
later, did not. Four arms dropped data silently: an unmappable segment (**10 rows**), an
id above `i64::MAX` (**10 rows**), an implausible price (1 row), and a failed ILP append
(1 row, because the `is_ok()` check had no `else` — the arm the dedicated drain does have).

The sharp part is not the loss, it is the **false assurance**. `DEPTH_COUNTER`'s own doc
states `refused` covers *"parse error, unmappable segment code, truncated frame tail, or an
ILP append failure"* — so an operator auditing `tv_dhan_feed_depth_total{outcome="refused"}`
would conclude d5 losses were visible when they were not. The counter documented coverage
the code never delivered.

**Reachable today, not latent:** the depth writer exists, the boot site wires
`with_inline_depth` unconditionally, and with the main feed in Full mode every code-8
packet reaches these arms on every tick.

Fixed with the EXISTING counter — no new metric name, which matters because
`user-data.sh.tftpl` has **zero free bytes** against its budget. No new pager either: the
Dhan noise lock makes a new Telegram page a REJECT without a dated operator quote.

### 17b — a cap that manufactured the signal it was meant to bound

The 2026-08-22 per-sid ceiling gated the MIRROR insert while `risk.record_fill` stayed
unconditional — deliberately unconditional, because refusing a fill would hide a leg we
actually hold. So past the ceiling risk held a position and the mirror had no key, and
`local_reconcile` reads a missing mirror key as `0`. The result: a **permanent
self-inflicted divergence**, firing OMS-GAP-02 on every reconcile cycle for the rest of the
day, and raising a floor that would mask a genuinely lost fill.

It also broke the invariant the file states in its own type doc — *"mirror + risk are
mutated together in `apply_fill`"* — which is the property leg 1 depends on for meaning.

**The mirror needs no bound of its own.** It gains a key only where risk gains one, so it is
bounded transitively at `MAX_TRACKED_POSITIONS + in-flight`, and risk halts with
`PositionCapacityExhausted` at the ceiling, which stops the inflow. `can_admit_sid` now
counts the TRIPWIRE alone; it used to take `max(tripwire, mirror)`, which — with the mirror
free to grow — would have let the mirror's size refuse tripwire slots the tripwire had room
for. Two maps, two growth axes, two bounds: conflating them is the mistake `oms/engine.rs`
already had to revert once this week.

Honest residual, unchanged: past the tripwire ceiling a new sid's fill still flows, with no
I-P1-11 cross-segment check for that id today. Counted, logged, and narrower coverage —
never a dropped fill.

### 17c — a stale time in an operator-facing field

The arming line carried a hardcoded `run_at_ist = "15:31"` that survived the Item 16 CAS
correction by three constants. It now derives. A literal in an operator-facing field is the
same class as a literal in a comparison window; it just fails more quietly, by telling the
operator a time the code no longer uses.

### What the parallel review REFUTED, recorded because it was my proposal

A hostile pass was run against my own proposed cross-verify scaling fix and killed four of
its five parts: a bounded `max_targets` breaks the "verify exactly what you captured"
doctrine and the test that enforces it; a `security_id IN (...)` filter re-admits the
I-P1-11 cross-product and is not expressible for a 24,600-id list in a GET query; refusing
on truncation is a REGRESSION because `Degraded` is excluded from `is_measured()`, so the
keep-better guard would leave a stale prior day's verdict standing; and excluding
REST-absent instruments would have MASKED the highest-value finding the comparator can
produce — a wrong or stale `security_id`. None of it shipped. The remaining cross-verify
defects (target scaling, `missing_rest` conflation at the verdict line, live-read pagination,
and the shared Data-API limiter bypass) are recorded in Item 18 and are NOT fixed here.

---

## Item 18 — four more live defects from the parallel permutation sweep (FIXED)

- [x] **18a — a poison timestamp on an untraded tick stamped a year-2106 partition**
  - Files: `crates/trading/src/candles/multi_tf_aggregator.rs`, `crates/storage/src/tick_persistence.rs`
  - Tests: `an_untraded_sentinel_with_a_poison_timestamp_is_refused_outright`,
    `an_all_ones_exchange_timestamp_never_stamps_a_year_2106_partition` (both bite-proven)
- [x] **18b — a NaN `average_traded_price` reached ILP**
  - Files: `crates/storage/src/tick_persistence.rs`
  - Tests: `a_non_finite_average_traded_price_becomes_null_and_never_refuses_the_tick` (bite-proven)
- [x] **18c — the cross-verify labelled every target `INDEX`: a partial-denominator vacuous pass**
  - Files: `crates/app/src/dhan_feed_stack.rs`
  - Tests: `an_equity_is_never_targeted_as_an_index_and_fno_is_never_guessed` (bite-proven)
- [x] **18d — an out-of-range target id was coerced to security_id 0**
  - Folded into 18c; same function, same test.

### 18a — one malformed packet, a partition nothing can reach

`exchange_timestamp` is a raw `u32` off the wire, so `0xFFFFFFFF` is ~2106-02-07. Two
independent gates should have caught it and neither did:

* The aggregator's band check sat **below** the `p == 0.0` untraded-sentinel return. A packet
  with LTP 0 and a poison LTT returned early, so `refused_timestamp` stayed false. The drain
  classifies `untraded_sentinel` as a **candle-only** refusal — the row is still written.
* `row_timestamp_ist_nanos` had a **floor and no ceiling**, so the poison value became the
  row's DESIGNATED timestamp.

`ticks.ts` is the designated timestamp, so such a row lands in a far-future partition that
retention and archival — both keyed on the trading day — can never reach, while every
`max(ts)` and range scan over `ticks` silently includes it. The drain's own comment claims a
timestamp "beyond a 30-year band" refuses the whole tick; that claim was false on this path.

Fixed at both layers: the band check moved above every early return that can still produce a
persisted row, and the stamp gained the ceiling. The second is defence in depth on purpose —
the band belongs where the stamp is MADE, so a future writer cannot reintroduce the hole by
calling the helper directly.

### 18b — the gate that documented itself as closed

The finiteness loop covers five fields. `average_traded_price` is a **sixth** caller of the
same `opt_price` closure and was never in it. `NaN != 0.0` is true, and both
`f32_to_f64_clean` and `round_to_2dp` pass non-finite straight through — so a NaN went to the
wire. The parser proves Dhan sends it: `parser::quote` has its own
`average_traded_price.is_nan()` assertion.

The consequence is exactly the chain `TickRowError::PriceNotFinite` documents as **closed**:
QuestDB rejects the whole batch, `discard_pending` clears up to 1,000 good rows, the rescued
buffer spills, and the replay tier wedges behind a file it can never accept.

A non-finite OPTIONAL price now becomes NULL and is counted, rather than refusing the row the
way the five mandatory prices do. Refusing here would discard a tick whose LTP is perfectly
good — losing a tick to protect an auxiliary column is the wrong trade.

### 18c — the vacuous pass the zero-denominator guard cannot see

`crossverify_targets` stamped the literal `"INDEX"` on every target, and that string goes
verbatim into the Dhan REST intraday body. The live universe is ~119 indices plus ~750 NSE_EQ
constituents, so roughly **86% of every run's fetches asked for a stock as though it were an
index**. Those return no candles, land in `rest_failures`, and are never compared — while the
run can still report **Clean** on the handful of correctly-labelled indices.

The module's `minutes_compared > 0` guard cannot catch this: the denominator is **partial**,
not zero. And the function's own doc comment says the lane "can never verify a different
universe than it captured" — true of the id set, false of the instrument type, and the type
is what decides whether a fetch returns anything at all.

F&O is deliberately **not guessed**. `(security_id, segment)` is all the subscribe set
carries, and `NSE_FNO` could be `FUTIDX`, `OPTIDX`, `FUTSTK` or `OPTSTK`. An unverifiable
target is counted (`tv_dhan_xverify_targets_unverifiable_total`) and named in a `warn!`, not
fetched with a wrong label — because a wrong label is worse than an absent one: it fetches
nothing while looking like a fetch that failed.

### Still NOT fixed, recorded rather than half-done

* **`tv_dhan_feed_depth_total` has no alarm and no dashboard widget.** Item 17a made the d5
  refusals countable; nothing in CloudWatch reads the counter, so the fix is half-delivered.
  Seven more EMF-shipped metrics are in the same state.
* **`dhan_live_crossverify.rs` emits ZERO metrics**, and `DHAN-LIVE-XVERIFY-01` is not among
  the codes in `error-code-alarms.tf`. The lane's only ground truth reaches CloudWatch through
  no metric and no alarm.
* **The cross-verify target list still cannot be fetched in one run** (~870 sequential REST
  fetches against a 240 s budget). Pagination of the live read and wiring the leg to the
  shared Data-API limiter are both open.
* **`missing_rest` is still counted as divergence** at the verdict line, contradicting the
  module's own header.
* **The aggregator's resident space is 2x every figure recorded in the repo** — `AggregatorCell`
  holds TWO `[LiveCandleState; TF_COUNT]` arrays, so 24,600 instruments is ~152 MB, not the
  42/77 MB the host-sizing arguments are built on. Not a defect; an understated claim.
* **"Entire current day in RAM" is arithmetically impossible for the second-scale timeframes.**
  See Item 19.

---

## Item 19 — the operator's RAM requirement, measured against the host

**Not a defect. A contradiction between two stated requirements, with the arithmetic.**

The requirement is the entire current day's **ticks + seconds + minutes** always resident.
Measured against `spot_bar_store.rs::total_bars_per_day_all_tfs` and `LiveCandleState`'s real
128-byte width, at the authorized 24,600 instruments:

| Layer | Arithmetic | Resident |
|---|---|---|
| Minute-scale bars, whole day | 831 bars x 48 B x 24,600 | **~981 MB — fits** |
| Second-scale bars, whole day | 77,422 bars x 48 B x 24,600 | **~87 GiB — impossible** |
| Raw ticks, whole day | 25–80 M x ~90 B | 2.3–7.2 GB — not implemented at all |

The host is 32 GiB with QuestDB taking 8–16 GiB. Second-scale residency is **~2.7x the entire
machine** and ~5x the free budget. The store resolves this today with a single `continue` that
skips `is_second_scale()` timeframes — load-bearing, not a placeholder, and currently silent.

So O(1) SPACE and full-day second-scale residency cannot both hold. This is an operator
decision, not an executor one, and the honest options are: keep seconds as a rolling window
(what the code does today), retain seconds for a bounded instrument subset, or move to a host
where the arithmetic closes. Recorded here so the next session decides deliberately rather
than discovering it at the OOM.

---

## Item 20 — RAM residency: MEASURED, and three of my own numbers were wrong

**Status:** measured 2026-08-25 08:40 IST against the live box
(`i-0c3fe906dad5492fc`) over the **full 2026-08-24 session** — read-only
QuestDB queries via SSM. Supersedes every RAM figure this repo has carried.

The operator's instruction was explicit: *"Then without a real proof don't
tell me tgese dude okay"*. The 12 GB / 42 GB / 175 GB figures were all
unproven. These are readings.

### What one full session actually contains (2026-08-24, Monday)

| Term | Measured | Instruments |
|---|---:|---:|
| Ticks | 64,349,753 | 18,097 |
| Peak ticks in ONE second | **92,131** | — |
| Second bars (1s/5s/10s/15s/30s) | 22,336,216 | 10,199 |
| Minute+ bars (1m..1d) | 3,154,318 | 10,199 |
| Depth rows d5 | 601,944,729 | 21,374 |
| Depth rows d20 | 745,434,920 | 248 |
| Depth rows d200 | 183,272,000 | 9 |
| **Depth rows total** | **1,530,651,649** | — |

### Three corrections to my own earlier claims

1. **"11 unrequested second-scale timeframes are 58% of the bars."** FALSE.
   Measured: `candles_2s,3s,4s,6s..9s,11s..14s` all wrote **ZERO rows**. Only
   the five frames the operator asked for are populated. The module header
   already said so — capacity-1 placeholders, 768 B/slot — and I did not read
   it before claiming otherwise. There is nothing to remove.
2. **"32 B per tick."** FALSE — that was in my own measurement script's
   header, unproven. `size_of::<ParsedTick>()` is **112 bytes**, measured. A
   32 B compact record is a DESIGN TARGET that does not exist in this
   codebase. Every figure below uses 112.
3. **Depth is 24x the tick volume**, not a rounding term. 1.53 billion rows
   against 64 million. At the depth table's 72 B/row that is ~110 GB/day,
   which is the write-amplification source Item 15 was hunting.

### The answer to the operator's question

`RamBar` = 48 B (test-pinned, `spot_bar_store.rs:678`). `ParsedTick` = 112 B
(measured). Latest-book widths: d5 168 B, d20 648 B, d200 6,400 B.

| What you hold | At today's 18,097 | Scaled to 24,600 |
|---|---:|---:|
| Ticks | 7.21 GB | 9.81 GB |
| All bars | 1.22 GB | 1.66 GB |
| Depth — CURRENT book only | 0.004 GB | 0.004 GB |
| **Subtotal (decision path)** | **8.4 GB** | **11.5 GB** |
| Depth — full-day HISTORY | 25.1 GB | 28.9 GB |
| **Total with depth history** | **33.6 GB** | **40.4 GB** |

Host is 32 GiB = 34.4 GB, and QuestDB wants 8-16 GB of it.

**So: everything the operator asked for FITS — ticks, all seconds, all
minutes, and the live book for every instrument — at ~11.5 GB.** What does
NOT fit is retaining every historical depth SNAPSHOT in RAM, which is 28.9 GB
on its own and is not what an entry/exit decision reads.

### Scaling honesty

24,600/18,097 = 1.36x on ticks and bars; 24,600/21,374 = 1.15x on depth.
Stated as an assumption and an UPPER bound: today's set is indices and cash
stocks, which tick MORE than the option strikes that would make up the
remainder. Note depth already covers 21,374 instruments — MORE than the
18,097 that ticked, because a book can quote without trading.

### NOT claimed

The 92,131-tick peak second is measured but not attributed — it may be an
open burst or a boot replay, and which one changes the ring sizing argument.
Not measured: RSS of the running process, which is the only number that
proves the arithmetic above against reality.

### The finding that matters more than the arithmetic

The RAM store is **allocated at boot and has no live writer.**

- `dhan_feed_stack.rs` — the live lane — contains **zero** references to
  `SpotBarStore` or `append_sealed` (grep, whole file).
- The only production `append_sealed` call site in the workspace is
  `rest_candle_fold.rs:1514`, and `config/base.toml [rest_candle_fold]
  enabled = false`.
- `market_ram_store_boot::install_market_ram_stores` runs at boot
  (`main.rs:3141`) and `spawn_ram_store_stats_task` publishes residency
  gauges for a store nothing fills.

So the live path today is tick -> fold -> ILP -> QuestDB. Nothing lands in
RAM for a decision to read. The arithmetic above proves the budget is
comfortable; it does **not** prove the capability exists. Wiring the live
seal into `append_sealed` is a real change with its own design, not a flag.

---

## Item 21 — Write amplification: ROOT CAUSE MEASURED on the live box

**Status:** measured 2026-08-25 ~08:55 IST, live box, read-only. Item 15 asked
what causes ~31x write amplification. This answers it, and the answer is a
schema decision, not a tuning knob.

### Live readings (Verified)

| Reading | Value |
|---|---|
| Root filesystem | **171 GB / 200 GB = 86%** (was 80.1% on 2026-08-24) |
| QuestDB `db` dir | **139 GB** |
| `ticks` partitions | **199** |
| `market_depth` partitions | 28 |
| WAL backlog — `market_depth` | **8,061 txns behind** (seq 209,312 / writer 201,251) |
| WAL backlog — every other table | < 1,500, mostly < 900 |
| Suspended WAL tables | **none** |
| App RSS, 69 min uptime | **1.62 GB** of 31.5 GB host |
| Deployed SHA | `6bfa4246` = `origin/main` exactly |

### The mechanism (Verified in source + data)

1. `ticks` is `TIMESTAMP(ts) PARTITION BY HOUR WAL`
   (`tick_persistence.rs:486-506`).
2. `ts` is the exchange **last-TRADE time**, not receive time.
3. **6,433,267 ticks on 2026-08-24 — 10.0% of the day's 64.3M — have `ts`
   more than one hour behind `received_at`.**
4. Those are not late packets. For an illiquid option the last trade
   genuinely WAS hours ago, so the packet is correct and its designated
   timestamp is old.
5. But the designated timestamp decides the PARTITION. So one commit in ten
   reopens an already-closed hourly partition and rewrites it.

That is the amplification: 4,744 GB written against ~151 GB retained. It is
not a QuestDB misconfiguration and not the page-size setting — it is
append-vs-rewrite, caused by choosing business time as the designated
timestamp on a feed whose business time runs hours behind arrival.

### What this rules OUT

- **Not the indices.** `IDX_I` ticked 7,946,506 rows on 2026-08-24
  (NSE_FNO 45.3M, NSE_EQ 11.7M). The hypothesis that the refusals were
  indices is FALSE.
- **Not a stuck writer.** Zero suspended WAL tables.
- **Not memory.** 4.6 GB used of 31.5 GB; the app holds 1.62 GB.

### The one live backlog worth watching

`market_depth` is 8,061 WAL transactions behind while every other table is
under 1,500. Depth is 1.53 billion rows/day (Item 20) against 64 million
ticks — 24x — and its writer is the only one losing ground.

### Independent confirmation of Item 20's no-live-writer finding

App RSS is 1.62 GB after 69 minutes. If the RAM bar store were being filled
by the live lane, RSS would already be climbing toward the multi-GB figures
Item 20 computes. It is not, because nothing writes it.

### NOT claimed

The 31x figure itself is from Item 15's earlier reading, not re-measured
here. Whether moving the designated timestamp to `received_at` is the right
fix is a DESIGN decision with real consequences — it changes what range
scans mean and interacts with the DEDUP key `(ts, security_id, segment,
capture_seq, feed)`. Recorded as the measured cause, not as an authorized
change.

---

## Item 22 — QUEUED REQUIREMENT: the full day resident in RAM, at full subscription

**Operator, 2026-08-25 (verbatim, typos preserved):** *"see always ensure to
maximsie 25 k instruemnts with full mdoe for dpeth 5 and 250 isntuments for
depoth 20 and 5 isntuents for dpeth 200 and order websocket udpate conenctiosn
ticks alwys so all tehs eof the ucrrent daya shdou lbe alwuas in ram also
ensure woth rela time lvie rpoven gauarneted assure dsoltuion dud eokay?"*

### Half of this is already true — measured live 09:48 IST 2026-08-25

| Requirement | Live now | Verdict |
|---|---:|---|
| 16 WebSocket connections | 15 market-data + 1 order-update | **MET, exact** |
| Full mode + depth-5, target 25,000 | 21,498 | 86% — gap is master resolution, not connectivity |
| depth-20, target 250 | 248 | **MET** |
| depth-200, target 5 | 5 | **MET, exact** |
| Order-update channel | connected | **MET** |

### The half that is NOT true, and it is the whole ask

**Nothing writes the RAM store.** Verified: `dhan_feed_stack.rs` contains ZERO
references to `SpotBarStore` or `append_sealed`; the only production
`append_sealed` call site in the workspace is `rest_candle_fold.rs:1514`, and
`config/base.toml [rest_candle_fold] enabled = false`. Boot installs the store
(`main.rs:3141`) and publishes residency gauges for something nothing fills.

Independent confirmation: app RSS was **1.62 GB** after 69 minutes of uptime.
If the store were filling, RSS would already be climbing.

So today the path is tick -> fold -> ILP -> QuestDB, and a decision has nothing
in RAM to read.

### The budget, measured not estimated (2026-08-24 full session)

Widths from source: `RamBar` = 48 B (test-pinned, `spot_bar_store.rs:678`);
`ParsedTick` = **112 B** (compiled and printed, NOT the 32 B an earlier note
assumed).

| Term | At today's 18,097 | Scaled to 24,600 |
|---|---:|---:|
| 64,349,753 ticks | 7.21 GB | 9.81 GB |
| 22,336,216 second bars + 3,154,318 minute+ bars | 1.22 GB | 1.66 GB |
| Live book, every instrument (d5 168 B / d20 648 B / d200 6,400 B) | 0.004 GB | 0.004 GB |
| **TOTAL** | **8.4 GB** | **11.5 GB** |

Host is 32 GiB = 34.4 GB; QuestDB wants 8-16 GB. **It fits, with room.**

What does NOT fit is retaining every historical depth SNAPSHOT — 1,530,651,649
rows/session = 25.1 GB today, 28.9 GB scaled. A decision reads the CURRENT
book, not the 60 million books before it. Current books cost 4 MB.

### What the work actually is

1. Wire the live lane's sealed bars into `SpotBarStore::append_sealed` — the
   store, the ring shape and the eviction already exist and are tested; what is
   missing is the call.
2. Decide tick residency. The store holds BARS. Holding 64.3M raw ticks needs
   its own structure, and at 112 B/tick a compact record is worth designing —
   the 32 B figure quoted earlier in this plan was never measured and is
   withdrawn.
3. `spot_bar_store.rs:131` SKIPS second-scale timeframes (`if
   tf.is_second_scale() { continue; }`). The operator's five second frames
   (1s/5s/10s/15s/30s) are exactly what that skip excludes. This is
   load-bearing and currently unstated at the call site.
4. Keep the current book per instrument — cheap (4 MB) and the only depth term
   a decision needs.

### Proof obligation before this can be called done

Not "the arithmetic fits". The live gauge `tv_process_rss_bytes` must be shown
climbing to and holding near the projected figure across a full session, and a
read from the store must be shown returning the current day's bars. Until an
RSS reading exists at scale, every number above is arithmetic — correct
arithmetic, from measured inputs, but not a running system.

---

## Item 13 — the archive worklist starved the heaviest table (2026-08-25)

**Status: DONE.** Found by tracing why the prod volume kept filling after the
spill-ceiling fix in #1804 landed.

- [x] **Fair-share the per-run archive budget across tables**
  - Files: `crates/storage/src/partition_archive.rs`
  - Tests: `fair_share_tests::the_heaviest_table_is_not_starved_by_an_older_backlog`,
    `each_table_is_still_processed_oldest_first`,
    `a_zero_cap_means_no_cap_and_keeps_every_item`,
    `a_cap_smaller_than_the_table_count_still_reaches_more_than_one_table`,
    `the_output_is_deterministic_across_runs`,
    `an_empty_worklist_is_empty_and_does_not_hang`
- [x] **Count list failures in the cycle summary** (a silently unswept table
      was indistinguishable from a healthy one)
  - Files: `crates/storage/src/partition_archive.rs`
- [x] **Resurrect a test that had never run**
  - Files: `crates/app/src/dhan_depth_universe.rs`

### What was measured, not inferred

| fact | value | source |
|---|---|---|
| `market_depth` share of QuestDB | 174 GB of 196 GB | `du` on the box |
| its partition size | ~17 GB per HOUR | `du` per partition |
| its archive attempts in 12 h | **0** | CloudWatch, untruncated |
| `ticks` archive attempts, same window | 87 | same |
| `ticks` eligible partitions | 185, oldest `2026-07-30` | QuestDB `table_partitions()` |
| `market_depth` eligible partitions | 4, oldest `2026-08-24` | same |
| `max_partitions_per_run` | 200 | `config/base.toml` |

### The defect

`archive_and_drop_old_partitions` sorted the whole worklist by partition name —
a global date order across every table — then truncated to the per-run cap.
Older-backlog tables therefore consume the entire budget before a newer-backlog
table gets a single slot. With stuck `2026-06-02` candle partitions at the head
(they fail `s3_conflict` every run and are re-listed the next one) plus 185
`ticks` entries, the 200 slots were spent before `market_depth` was reached.
Deterministic, every run: the one table that could relieve disk pressure was the
one table guaranteed never to be archived.

### Honest envelope

- Fair-sharing makes `market_depth` reachable. It does **not** make the archiver
  keep up: ~8 partitions/day at ~17 GB is ~136 GB/day to gzip, upload and
  SHA-256 verify. Whether one session's budget clears one session's production
  is UNMEASURED and is the next question.
- The `s3_conflict` partitions still never converge. They no longer block other
  tables, but they still fail every run.
- Depth volume itself (~110 GB/day into a 300 GB volume) is a capacity decision,
  not an executor's.

### A test flaw the bite-proof caught

The first version of the headline test used 185 + 4 = 189 entries against a cap
of 200, so nothing was truncated and it passed under the OLD algorithm too —
i.e. it proved nothing. The reproduction now includes the June candle partitions
that pushed prod over the cap, and asserts `worklist.len() > 200` so it can
never silently become vacuous again. Bite-proven both directions: reverted, it
fails with `market_depth scheduled: 0`.

---

## Item 14 — the pre-open readiness alarm pages on every restart (2026-08-25)

**Status: DONE.** Found by reading why `tv-prod-preopen-ready-late` was in ALARM
and armed, hours after the eight #1809 alarms finally applied.

### What the live log actually said

```
09:08:04  attempts: 57  ready_at 32884  deadline 33120  met_deadline: TRUE
12:37:58  attempts:  1  ready_at 45478  deadline 33120  met_deadline: false
16:17:30  attempts:  1  ready_at 58650  deadline 33120  met_deadline: false
17:33:59  attempts:  1  ready_at 63239  deadline 33120  met_deadline: false
18:17:30  attempts:  1  ready_at 65850  deadline 33120  met_deadline: false
19:21:34  attempts:  1  ready_at 69694  deadline 33120  met_deadline: false
```

The morning MET the deadline with four minutes to spare. The five `false` rows
are restarts on a busy deploy afternoon; `ready_at_ist_secs` is simply "now".

### The defect

`met_deadline = ready_at <= PREOPEN_READY_DEADLINE_IST_SECS` asked "did this
attach finish before 09:12?" of EVERY attach, including ones that had not begun
until the afternoon. A restart cannot pass a test whose pass condition is a time
already in the past, so the verdict was decided by the wall clock rather than by
anything about the lane. Each restart fired `WS-GAP-02` and drove the alarm —
ungated by design — into ALARM.

The alarm's own comment reasoned that "on a restart day the LATEST attach is the
one that matters". True of a 10:30 re-attach; false of a 19:21 one. The missing
distinction is not WHEN the attach finished but whether it was ever racing the
open, and only the START second carries that.

- [x] **Gate the deadline verdict on whether the attach was racing the open**
  - Files: `crates/app/src/dhan_feed_stack.rs`
  - Tests: `contract_attach_tests::the_preopen_deadline_does_not_apply_to_a_mid_session_restart`,
    `an_attach_that_begins_before_the_deadline_is_still_judged_if_it_overruns`,
    `the_mid_session_arm_does_not_emit_the_field_the_alarm_filters_on`

### Why no terraform change

The CloudWatch filter is anchored on `{ $.fields.ready_at_ist_secs = * }`. The
mid-session arm reports its completion second as `attached_at_ist_secs`
instead, so it produces no datapoint at all. The alarm's threshold, statistic
and `notBreaching` semantics are untouched, and the gauge is skipped on that
arm for the reason its own doc already gives for the give-up paths: there is no
readiness second, and publishing the wall clock as one reads as a permanently
missed deadline.

### Guarantee matrix

Carried by reference from this plan's shared matrix (15-row + 7-row). Rows that
move for this item: **monitoring** (an armed alarm stops firing on normal
operation), **logging** (the mid-session arm gets its own line and field),
**alerting** (`WS-GAP-02` no longer fires on a restart), **scenarios**
(restart-after-deadline is now a covered case), **extreme check** (three tests,
all bite-proven). No hot-path, schema, DEDUP-key, or WebSocket-count change, so
the 7-row resilience matrix is unmoved.

### Honest envelope

- This removes a FALSE page. It does not make any attach faster, and a genuinely
  late pre-open still fires exactly as before — pinned by
  `an_attach_that_begins_before_the_deadline_is_still_judged_if_it_overruns`.
- Keyed on the attach START, so an attach beginning at 09:11 and finishing at
  09:20 is still judged. Excusing that would hollow the alarm out from the other
  side while fixing the false pages.
- UNVERIFIED-LIVE: the fix is proven in tests and by source scan against the
  real filter pattern; that the alarm actually stays quiet across tomorrow's
  restarts is measured tomorrow, not claimed here.

---

## Item 15 — the silence detector was blind to 96% of the universe (2026-08-25)

**Status: DONE.** Found while verifying that the 2026-08-21 IDX_I Quote-mode fix
had worked. It had (`never_ticked: 0`, vs 119 the day before) — but the same log
lines carried the real finding.

### Measured

```
08:31:09  refused: 1,276,658  tracked: 865
12:37:47  refused: 1,211,764  tracked: 865
```

865 is exactly the spot universe. Today's subscribed set was **23,044**
(868 spot + 22,176 contracts).

### The defect

`LiveIngest::new(writer, capacity)` uses one number for two things with
opposite semantics: a SOFT pre-size for the fold, and a HARD cap for the
detector (`TickGapDetector::with_capacity` — "Never grows; never reallocates").
The boot site computes `capacity` from the main-feed set BEFORE any socket
opens, which is the spot universe; the ~22,000 contracts attach minutes later
via `run_contract_attach`. Every contract tick was then refused a slot.

Consequence, in the detector's own words: `scan_silence`'s `silent` and
`never_ticked` counts "describe only the instruments it still tracks, while
reading exactly as though they describe all of them". A contract that was
silently never subscribed could not be reported by anything.

- [x] **Size the detector at the authorized ceiling, not the boot-time count**
  - Files: `crates/app/src/dhan_feed_stack.rs`
  - Tests: `tests::the_detector_outlives_the_boot_time_universe_when_given_the_ceiling`,
    `a_detector_at_its_ceiling_still_refuses_and_still_counts`,
    `the_production_boot_site_sizes_the_detector_at_the_authorized_ceiling`

### Why the ceiling and not a bigger boot-time count

The ceiling does not depend on WHEN the universe is counted, so this cannot
silently re-break the next time instruments are added after boot — which is
exactly how it broke the first time. Cost: ~2 MB of slots plus its index,
against a 32 GiB host.

### Guarantee matrix

Carried by reference from this plan's shared matrix. Rows that move:
**monitoring** (silence detection now covers the whole subscribed set),
**scenarios** (grow-after-boot is a covered case), **extreme check** (three
tests, bite-proven). Fail-closed behaviour at the ceiling is explicitly
preserved and pinned — a tracked instrument is still never evicted.

### Honest envelope

- This makes the detector ABLE to see contracts. It does not prove any
  contract is actually ticking, and it adds no alarm: the refusal gauge is not
  EMF-selected (user-data byte budget), so the coded log line remains the only
  operator surface.
- It does not address loss UPSTREAM at the vendor: Dhan publishes no sequence
  number and skips a slow consumer forward, so ticks discarded on their side
  are invisible to every counter we own. The 15:31 cross-verification remains
  the only ground truth for that class.

---

> **MERGE NOTE 2026-08-25 (second occurrence).** Two items were appended to
> this file in parallel and the numbering collided again — main's silence-detector
> item is numbered 15 while the audit-findings item below is numbered 23. Both
> are kept verbatim; neither is renumbered, for the same reason the 2026-08-25
> note above the two "Item 12" sections gives: a plan file is an audit trail,
> and renumbering a landed item breaks every reference to it. Read them by
> title, not by number.

## ITEM 23 — The nine audit findings, fixed (2026-08-25, operator: "fix and resolve evrythignd dude okay? meanhwiel ensure to pull the latest merged changes and only then do ti dude okay? i mean fix and resolve all of them dude okay?")

The 2026-08-25 five-agent audit produced nine verified findings. This item
closes all nine. Seven of them were the SAME defect: a table, a constant or a
comment that was declared, given a schema or a doc, and never wired to
anything — invisible precisely because it looks finished.

| # | Finding | Disposition |
|---|---|---|
| 01 | The 15:41 cross-verify — the feed's only proof of correctness — was logged and discarded. Both writers and the DDL had ZERO callers, and a comment claimed the opposite | WIRED + comment retracted |
| 02 | `instrument_lifecycle_audit`, the 5-year regulatory chain, had no writer | WIRED |
| 03 | The disk alert was a fixed 10 GiB justified as "~10% of the 100 GB volume"; the volume is 300 GB | DERIVED from the measured volume |
| 04 | 22,996 of 25,000 slots used; overflow refuses the WHOLE subscription | HEADROOM WARNING added |
| 05 | Fourteen tables stopped applying rows on 2026-08-25 and the alarm stayed silent | LAG DETECTOR + BLIND SENTINEL |
| 06 | 305 of 381 metrics never leave the box; blocked at 0 free user-data bytes | 2,515 BYTES FREED |
| 07 | Array indexing was not lint-blocked on the packet-decode path | DENIED, 23 reviewed allows |
| 08 | `MAX_DAILY_UNIVERSE_SIZE` had zero production readers | READ, with a drift warning |
| 09 | The reset command destroys the SEBI tables with no export | EXPORT-OR-ABORT added |

## Design (Item 23)

Each fix is the smallest change that makes the declared thing true, and every
one is pinned by a test that fails the build if it is undone.

**01** — `CrossverifyDeps` gains the ILP write config; `persist_xverify_report`
writes findings + a daily row after every run, INCLUDING vacuous ones ("we
could not measure today" is a fact about the feed). `ensure_dhan_live_crossverify_tables`
is called at boot beside the ticks/depth DDL, so ILP cannot auto-create either
table without its DEDUP key.

**02** — `KnownInstrument` carries the full prior `LifecycleState`, not just a
boolean. `classify_lifecycle_transition` is a NEW classifier rather than a
reuse of `lifecycle_reconciler::classify_transition`, deliberately: that one
answers "is the row present in today's CSV?", and the Dhan master keeps expired
contracts IN the file with a past expiry date — so the reconciler returns
`None` for the single most common transition in the product.

**03** — `spill_disk_free_critical_threshold(total)` = `max(10 GiB, total/10)`.
The floor means the threshold can only move UP relative to today.

**04/08** — `report_universe_headroom` warns below 10% free and cross-checks
the caller's capacity against `MAX_DAILY_UNIVERSE_SIZE`, which had no reader.

**05** — `WalLagTracker` flags tables whose apply lag is above a floor AND
non-decreasing for five consecutive polls. Growth, not magnitude: this repo has
never measured a normal session's peak lag, and guessing an absolute number is
the failure being corrected. Separately, five consecutive probe failures set
the suspended-tables gauge to −1 instead of leaving a stale `0` standing.

**06** — user-data writes a minimal host-only agent config and copies
`deploy/aws/cloudwatch-agent.json` into place after the Step 5 clone. One copy
cannot drift from itself, and the lockstep guard inverts to pin the new shape.

**07** — `deny(clippy::indexing_slicing)` on the nine Dhan parser modules,
`cfg_attr(not(test))` like the existing unwrap/expect bans. 23 reviewed allows.

**09** — Both destructive actions export the four SEBI tables to a directory
outside the volume before touching it, and ABORT if a table exists and cannot
be exported.

## Edge Cases (Item 23)

- Vacuous/blind cross-verify run → still persisted, stamped from the run, not `now()`.
- No findings → daily row still written, no fabricated timestamp.
- Rerun of either writer → UPSERTs, because both DEDUP keys carry the run stamp.
- Unchanged instrument → NO audit row (else the chain becomes a 150,000-row daily snapshot).
- `Delisted` → never auto-audited; it is an operator-set terminal state.
- Small dev disk → keeps the 10 GiB floor; degenerate `total = 0` cannot disarm the alarm.
- Busy-but-healthy table with oscillating lag → never pages (asserted with ten oscillating samples).
- Missing `writerTxn`/`sequencerTxn` → no lag verdict, never a fabricated zero.
- QuestDB unreachable during a reset → export skipped LOUDLY and the reset proceeds; it is also the remedy for a wedged QuestDB.
- Clone fails → host metrics still flow from the fallback config.

## Failure Modes (Item 23)

| If this breaks | Consequence | Bound |
|---|---|---|
| xverify persist fails | today's comparison exists only in the log | pages via the alarmed `xverify_failed` label |
| lifecycle audit write fails | today's history missing; STATE unaffected | logged + counted, never fails the state write |
| lag detector threshold wrong | false pages, or a missed stall | edge-latched; growth-based, so a catching-up table clears it |
| blind sentinel wrong | gauge reads −1 while healthy | one successful probe clears it |
| SEBI export wrong | a reset destroys regulatory data | ABORTS rather than proceeding |
| CW agent copy fails | app metrics absent | host metrics still flow; two WARNING lines say which |

## Test Plan (Item 23)

`derived_threshold_scales_with_the_volume` (5 cases + monotonicity) ·
`growing_lag_fires_on_a_stuck_table_and_stays_quiet_on_a_busy_one` (the
2026-08-25 shape replayed, plus ten healthy oscillations) ·
`a_recovered_table_can_page_again_on_a_second_episode` ·
`the_blind_gauge_sentinel_can_never_collide_with_a_real_reading` ·
`the_daily_row_carries_every_comparison_total` (distinct values per field so a
transposition cannot pass) · `the_cross_verification_findings_are_actually_persisted` ·
`the_audit_chain_records_every_state_change_and_nothing_else` ·
`a_delisted_instrument_is_never_audited_automatically` ·
`the_documented_universe_ceiling_is_actually_read_and_matches_its_derivation` ·
`destructive_actions_export_the_sebi_tables_before_destroying_the_volume`
(asserts ORDER, not just presence) · `user_data_carries_no_second_copy_of_the_selector` ·
`the_deployed_agent_config_is_the_repo_file_and_has_no_second_copy`.
The indexing deny is bite-proven: removing one allow produces 8 errors.

## Rollback (Item 23)

Every change is additive or a guard inversion; `git revert` restores the prior
behaviour with no schema migration. The two new tables are `CREATE TABLE IF NOT
EXISTS` and are never read by anything else, so leaving them behind costs
nothing. The user-data change is the only one that touches the boot path: the
fallback config means a revert is never required to keep host metrics flowing.

## Observability (Item 23)

New: `tv_dhan_feed_xverify_rows_total`, `tv_dhan_feed_xverify_persist_errors_total`,
`tv_instrument_lifecycle_audit_rows_total` — all local-only, deliberately NOT
EMF-selected. Each already pages through an existing log filter, and a metric
shipped with nothing watching it is the paid-for-and-unwatched shape these
alarms were added to end. One new CloudWatch alarm,
`tv-<env>-wal-suspension-probe-blind`, on a gauge that is already selected:
~$0.10/mo, no new metric name, no user-data byte.

### Per-Item Guarantee Matrix (Item 23)

Cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row +
7-row). Coverage: 12 new tests across 5 crates; audit coverage: two tables that
had no writer now have one; performance: no hot-path change (the indexing deny
generates identical code); security: no new external surface, the SEBI export
writes locally with no credentials; O(1): the lag tracker is O(tables) on a
60s timer over ~30 rows, the threshold derivation is O(1), the audit builder is
O(rows) with one hash lookup each — all cold path.

**Honest envelope:** 100% inside the tested envelope, with ratcheted regression
coverage. NOT claimed: that the WAL-suspension silence of 2026-08-25 is now
explained — the recorded txn evidence is consistent with the lag path and the
blind-probe path, and the day cannot be replayed, so BOTH are fixed rather than
one being declared the cause. NOT claimed: that the lag threshold is calibrated
— no normal-session baseline exists, which is why the signal is growth and not
magnitude; the first session with real lag data is what validates it. NOT
claimed: that the user-data restructure is live-verified — no instance has
booted with it, and the fallback exists precisely because that is unproven.


---

## Item 24 — the ILP flush leaves the frame-drain task (2026-08-25)

- [x] `TickWriter::split_for_offload` — producer keeps the buffer and the row
      accounting; a new `TickWriterSink` takes the ILP `Sender`
  - Files: `crates/storage/src/tick_persistence.rs`,
    `crates/app/src/dhan_feed_stack.rs`
  - Tests: `offloaded_flush_hands_the_rows_off_and_does_not_touch_the_network`,
    `a_full_queue_keeps_the_rows_and_never_reports_them_as_dropped`,
    `the_sink_reports_zero_rows_when_the_flush_fails`,
    `a_writer_that_was_never_split_behaves_exactly_as_before`,
    `an_offloaded_flush_never_touches_the_network_from_the_drain`,
    `the_writer_thread_is_joined_at_shutdown_so_the_tail_batch_lands`,
    `test_drain_never_flushes_bare_on_the_async_worker` (re-blessed, 3 bites)

### Design

`TickWriter::flush` is a synchronous ILP-over-HTTP round trip with a 5 s
timeout, and it ran ON the frame-drain task. `blocking_flush`/`block_in_place`
bounded the damage to the RUNTIME — the other tasks keep their workers — but
did nothing for the drain itself, and the drain is the only consumer of the
frame ring. So a slow database stopped the fold, the socket receive buffer
filled, and Dhan — whose published behaviour is to skip a slow consumer forward
to "the latest available state", with no sequence number — discarded the
intermediate ticks at THEIR side. That loss is invisible to every counter we
own, and no amount of provisioned disk throughput removes it, because the
coupling is structural rather than a matter of speed.

The writer is now split across a bounded `sync_channel` (`FLUSH_QUEUE_DEPTH`
= 4) with a named OS thread `tv-tick-writer` on the far side. The drain's flush
becomes a `try_send`.

### Edge Cases

| Case | Behaviour |
|---|---|
| Queue full | `OffloadOutcome::QueueFull` — rows RETAINED and still pending, `Ok(())` returned, next flush retries. Counted by `tv_tick_flush_queue_full_total`. Never reported as loss, because none happened. |
| Queue full past `MAX_PRODUCER_BUFFER_BYTES` (64 MiB) | Stops accumulating and rescues through the existing `discard_pending` path — durable, counted, re-ingestable. Bounds the memory an hour-long stall can consume. |
| Writer thread gone | Rescue + `Err`, never a silent success. |
| Writer never split | Byte-for-byte the previous synchronous behaviour, pinned by its own test. |
| Shutdown | `shutdown_offload_writer` closes the queue and JOINS, after the tail flush. |

### Failure Modes

The tail flush was the sharp one: it hands the last batch to the queue, so
without a join the process could exit with that batch in flight — re-creating,
one queue further out, exactly the loss the tail flush exists to prevent. The
join handle therefore lives inside `LiveIngest`, where the drain that owns the
tail flush can reach it, and a source-scan assertion pins the call.

Spawn failure is non-fatal and non-silent: the lane falls back to the
synchronous path with a coded error. Degraded beats refusing to boot.

### Test Plan

132 test binaries green across `tickvault-storage` + `tickvault-app` with
`--no-fail-fast`, zero failures. The structural guard was bite-proven three
ways — a bare blocking flush, a never-wired boot site, and a removed shutdown
join each turn it red, and each restores green.

### Rollback

Delete the `spawn_offload_writer` call at the boot site. Everything else is
inert: `flush` branches on `offload.is_some()`, which is `None` on every
constructor.

### Observability

New: `tv_tick_flush_offloaded_total`, `tv_tick_flush_queue_full_total` — local
exporter only, deliberately NOT EMF-selected and NOT alarmed. **The loss path
they sit on is already alarmed**: a stall long enough to matter crosses the
producer ceiling and increments `tv_ticks_dropped_total`, which carries
`tv-<env>-ticks-dropped`, plus `tv_ticks_spilled_total` and the coded
`HOT-PATH-02` error. A transient full queue is backpressure, not loss, and
paging on it would train the operator to ignore the counter that means loss.
No new rule row is needed because no new page is added.

### Per-Item Guarantee Matrix (Item 24)

Cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row +
7-row). Coverage: 6 new tests + 1 re-blessed guard, bite-proven 3 ways. Zero
tick-drop path added — the queue-full arm is the proof, and it retains rows.
O(1): the hand-off is one `try_send` and one `mem::replace`; no allocation is
added to the per-tick path (the replacement `Buffer` is minted once per FLUSH,
at 500 ms cadence, not per row).

**Honest envelope:** 100% inside the tested envelope, with ratcheted regression
coverage: the drain's flush no longer performs network I/O, a full queue
retains rows rather than blocking or dropping, and the session tail is joined
before exit. **NOT claimed:** that this removes tick loss — it removes ONE
mechanism (drain stall → socket backpressure → vendor-side skip-forward), and
the vendor's skip-forward remains undetectable to us because the India feed
carries no sequence number. **NOT claimed:** that it is measured live — no
instance has run it; the ~25× write amplification that makes the disk saturate
in the first place is untouched and is the next item. **NOT claimed:** clippy
clean locally — the component is not installed in this container; CI's Build &
Verify is the check.

### Item 24 — compliance with Item 14d's five binding conditions

Item 14 recorded this design on 2026-08-25 and bound the implementing PR to
five conditions. Audited one by one rather than asserted:

| | Condition | Status |
|---|---|---|
| **C1** | Overflow SPILLS, never a silent drop, never a blocking append on the fold path | **PASS** — `try_send`, never `send`; every cut routes through the existing rescue tier |
| **C2** | The drain ships pre-stamped rows; the writer never mints `capture_seq` | **PASS** — `FlushBatch` carries a finished `Buffer`; the sink only calls `sender.flush()` |
| **C3** | `record_ticks` and `LAST_TICK_AGE_GAUGE` move to the writer | **PASS, by construction** — `record_ticks` is on the thread, and the gauge is *derived* from `feed_health.last_tick_age_secs`, so it follows the registry the writer stamps. `no_ticks_alarm_gauge_guard.rs` passes unchanged, so no re-bless and no dated §2.3b edit are needed — the alarm still means "rows persisted" |
| **C4** | Bounded shutdown grace; batch width capped; `max_buf_size` headroom const-asserted | **PASS — and this was the one the first draft FAILED** |
| **C5** | A size cut must fire well before the 100 MiB questdb-rs wedge | **PASS** — `MAX_PRODUCER_BUFFER_BYTES` is const-asserted at ≤ half of `QUESTDB_MAX_BUF_SIZE_BYTES` |

**C4 is worth writing out, because the first draft of Item 24 shipped without
it and would have made the disk problem worse.** Item 14's hostile pass had
flagged the own-goal: a decoupled writer accumulates under pressure, so each
commit spans a wider row range — and wider commits are what the amplification
is made of. It bound the implementation to cap width *until 14b(3) was
measured*. Item 15/f638bb66 then measured it: `ticks` is `PARTITION BY HOUR` on
the exchange last-TRADE time, and **10.0% of one day's 64.3M ticks carried a
`ts` more than an hour behind arrival**, so one commit in ten reopens a closed
hourly partition and rewrites it. **Commit width is the amplifier**, which is
the branch of 14b(3) that makes the cap mandatory rather than optional.

Three fixes followed:

1. `MAX_RETAINED_FLUSH_SPANS = 2` — the producer may retain across two flush
   spans, then stops widening and spills. Enforced on the retain path, not
   merely declared, and pinned by the structural guard.
2. `OFFLOAD_SHUTDOWN_GRACE = 30 s` — `JoinHandle::join` has no timeout, so a
   writer wedged on a hung socket would have hung the box's shutdown. Worse,
   on a host whose auto-stop is a cost control, than losing the tail batch.
3. `MAX_PRODUCER_BUFFER_BYTES` 64 MiB → **32 MiB**. The const assertion
   *refused to compile* at 64 — the assertion doing its job, on the first
   number I picked.

`OffloadOutcome::WidthCapped` is a distinct arm from `SinkGone` because the
writer is alive and well during a width cut; reporting "the writer thread is
gone" would send an operator to diagnose a healthy thread.

**14e** (no new EMF metric name may ship — `user-data.sh.tftpl` at zero free
bytes) is honoured: all three new counters are local-exporter only. The loss
path they sit on already pages through `tv_ticks_dropped_total`.

---

## Item 25 — three dead loss-signals on the durable floor (2026-08-26)

**Operator instruction (2026-08-26, verbatim, typos preserved):**

> "i believe pre open percetage cjmaeh and percnetahe change both of them not
> needed for ticks dude it is oen and only need for seocnds an dminutes dude
> okay? see emanwhiel hwow ill you ensure that evn seconds level also will
> hodl thes eprecise ohlcv and even minute laso ohlcv dude okay? can you lsit
> i tour dud eoaky?"

He is narrowing an earlier request: the percentage change and the pre-open
percentage change belong on the **seconds and minutes frames**, not on the raw
tick table. That is exactly where the columns already are — and that is what
makes the finding below worth an item rather than a shrug.

### The finding (MEASURED live, not inferred)

`candles_<tf>` has carried `open_pct`, `open_gap_pct`, `change_pct` and
`close_pct_from_prev_day` since the Wave-5 seal-column work. Read live on the
production box, 26 Aug 2026, market session only:

| Frame | Bars stored today | With `open_pct` ≠ 0 | With `open_gap_pct` ≠ 0 |
|---|---:|---:|---:|
| `candles_1s` | 9,038,254 | **0** | **0** |
| `candles_5s` | 4,236,485 | **0** | **0** |
| `candles_15s` | 2,399,970 | **0** | **0** |
| `candles_1m` | 1,065,763 | **0** | **0** |
| `candles_3m` | 520,649 | **0** | **0** |
| `candles_15m` | 148,183 | **0** | **0** |

17.4 million bars, six frames, not one value. The cause is not a plumbing gap
and not a schema gap — both were verified present:

- **The baselines are live.** `prev_day_close` and `session_open` are
  refreshed from the exchange fields on every fold in `fold_into_bucket`,
  last-non-zero-wins. In the minute sampled, all **189,396** ticks carried
  both.
- **The write path is intact.** `ShadowSealRow::from_buffered_seal` copies
  `state.open_pct` / `state.open_gap_pct` / `state.close_pct_from_prev_day`
  verbatim into the ILP row, and the columns exist in the DDL.
- **The arithmetic was never written.** `open_bucket` initialises all three to
  `0.0`, and a workspace scan finds **no other production assignment to any of
  them**. The doc comments on `LiveCandleState` say "Stamped at seal time";
  nothing stamps.

This is the false-OK class: a column that exists, is documented, flows through
the writer, and is always zero reads to a consumer as "this instrument has not
moved", not as "this was never computed".

### Design

One pure method on `LiveCandleState`, called at every point a bar leaves the
aggregator:

```
stamp_seal_percentages():
    close_pct_from_prev_day = pct(close,        prev_day_close)
    open_pct                = pct(close,        session_open)
    open_gap_pct            = pct(session_open, prev_day_close)
```

where `pct(value, baseline)` returns `0.0` unless `baseline` is finite and
strictly positive **and** `value` is finite, and returns `0.0` again if the
quotient is not finite. Zero is the existing "not computable" sentinel for
these columns and stays so — a bar whose baseline never arrived is
indistinguishable from one that has not moved, and inventing any other value
would be worse.

`volume_pct_from_prev_day` and `oi_pct_from_prev_day` are deliberately NOT
stamped: their columns were dropped from the candle DDL (operator decision
2026-05-28 — spot instruments have no OI and indices have no volume), so
computing them would produce fields nothing reads.

**`change_pct` is not a fourth number.** `ShadowSealRow` already sets it equal
to `close_pct_from_prev_day`; that stays, and this item does not add a
separate meaning for it.

### The emission sites — all four, and why a helper is not enough

A bar leaves `AggregatorCell` at four points, and the fourth is the one a
careless fix would miss:

| # | Site | Why it must stamp |
|---|---|---|
| 1 | `consume_tick`, intraday bucket crossing (`ConsumeOutcome::Sealed`) | the ordinary path — most bars |
| 2 | `force_seal` | the IST-midnight / shutdown drain |
| 3 | `catch_up_seal` | the watermark sweep, which seals illiquid instruments on time |
| 4 | `consume_tick`, late refold (`ConsumeOutcome::AmendedLate`) | **the bar's `close` CHANGED.** A percentage stamped before the amend is stale for the row that is actually persisted |

Site 4 is the reason this is not a one-line change. A late tick re-writes
`close` on an already-emitted bar and the amended state is re-persisted; a
percentage computed at the original seal would then disagree with the `close`
sitting beside it in the same row.

### Edge Cases

| Case | Behaviour |
|---|---|
| Pre-open, no exchange baseline yet | `session_open` / `prev_day_close` are `0.0`; both percentages stamp `0.0`. Unchanged from today. |
| Baseline arrives mid-session (indices — the first several ticks carry no open) | last-non-zero-wins already handles it; every bar sealed after arrival stamps correctly. Bars sealed before it keep `0.0`, honestly. |
| `close` is a subnormal or non-finite value | the ingest gates upstream already refuse those; the `is_finite` test here is belt-and-braces and returns `0.0`. |
| Baseline is negative or non-finite | refused — `0.0`. A negative baseline would flip the sign of every percentage silently. |
| Quotient overflows to infinity | refused — `0.0`. Reachable only from a subnormal baseline that passed the positivity test. |
| A late refold changes `close` | site 4 re-stamps from the amended `close`. |
| Day boundary | `force_seal` clears the day-scoped state as it already does; the next day's first bar stamps `0.0` until its baseline arrives. |

### Failure Modes

| If this breaks | Blast radius |
|---|---|
| The stamp is wrong | a wrong percentage in a column that is zero today — strictly no worse than the status quo, and visible immediately in any query |
| A fifth emission site is added later without stamping | that path silently ships zeros again. **This is the real risk**, and it is what the guard test below exists for. |
| The stamp panics | it cannot: no indexing, no unwrap, no division without a positivity test |

### Test Plan

Unit tests on `LiveCandleState::stamp_seal_percentages` (pure, no I/O):

- a normal bar stamps all three correctly against hand-computed values
- the live NIFTY numbers from today used as a fixture
  (open 24,341.95 · prev close 24,334.55 · close 24,273.15 →
  intraday −0.28%, gap +0.03%)
- zero / negative / non-finite / subnormal baseline each stamps `0.0`
- non-finite `close` stamps `0.0`
- an unchanged price stamps exactly `0.0`, not a rounding artefact

Aggregator tests, one per emission site:

- an intraday crossing seal carries non-zero percentages
- `force_seal` carries them
- `catch_up_seal` carries them
- a late refold that moves `close` re-stamps to match the amended `close`

Guard test (the durable half):

- a source scan asserting every `ConsumeOutcome::Sealed` / `AmendedLate`
  construction and every `mem::replace` seal in `aggregator_cell.rs` is
  preceded by a stamp — so a fifth path added next month fails the build
  instead of shipping zeros

### Rollback

Revert the commit. The columns return to `0.0`, which is the state they have
been in since they were created — no migration, no schema change, no data to
undo. Nothing downstream reads them today, so nothing breaks in either
direction.

### Observability

No new metric, no new alarm, no EMF entry. It makes an EXISTING operator surface
tell the truth.

### Honest envelope

Fixes the `connected` dimension only. The sibling `freshness` gap named in the
same guard doc — a CONNECTED-but-silent feed reading healthy — is separately
covered by the 30 s silence scan (`RISK-GAP-03`), which has paged since
2026-08-15. What this does NOT do is make `/health` itself see silence: it
consults neither tick flow nor the feed registry, so 15 alive sockets receiving
nothing still reads `healthy` there. The two surfaces had opposite defects and
only one is fixed here.

---

## Item 30 — the offload left a 5-second blocking HTTP call on the drain

### Design

`flush_and_record` early-returns `ingest.flush()` **bare** when the tick writer
has been offloaded, on a comment reading *"the flush is a bounded-queue hand-off
with no network in it, so there is nothing to move off-worker"*.

That is true of the tick writer and false of the callee. `LiveIngest::flush`
flushes the inline-depth sink FIRST and unconditionally, above its own
`pending_rows == 0` early return — its body says so and explains why — and
`DepthIngest::flush` is a blocking ILP-over-HTTP round trip bounded by the
conf-pinned `request_timeout=5000`.

So the offload, landed on 2026-08-25 precisely to get blocking HTTP off the
drain task, left blocking HTTP on the drain task, on the **only path production
takes**: the boot site calls `spawn_offload_writer` and `with_inline_depth` on
the same builder chain, so both halves are live on every real run and this early
return was reached on every flush.

**Fix:** wrap it. `return blocking_flush(|| ingest.flush());`

Wrapped unconditionally rather than gated on "is there depth pending": a gate
needs a second accessor kept in step with what `LiveIngest::flush` actually
does, and keeping those two pieces of knowledge in separate places is what
produced this bug. The cost of being wrong the cheap way is one worker swap at
~5 flushes/sec; the cost of being wrong the other way is a 5-second worker pin.

### Edge cases

- **Current-thread runtime** — `block_in_place` panics there, which is why
  `blocking_flush` guards on `runtime_flavor()`. This module's drain tests are
  bare `#[tokio::test]`. Unchanged by this item; the helper already handled it.
- **Depth buffer empty** — the wrap still happens. Deliberate, see above.
- **Non-offloaded fallback** — already wrapped; untouched.
- **Flush failure** — unchanged. `LiveIngest::flush` counts and logs the
  inline-depth failure and does not propagate it, so the tick flush is never
  skipped because the depth writer had a bad moment.

### Failure modes

The mechanism is the one `blocking_flush`'s own docs describe, and it is not
"the flush is slow": a stalled QuestDB pins a tokio worker for up to 5 s; on a
low-worker host that is a large fraction of the runtime; the WS read loops
sharing that worker stop pumping pongs and the socket is dropped; the drain
stops draining and the ring fills. A third-party database stall becomes tick
loss plus a disconnect.

### Test plan

Two tests, and the split is the point.

1. `test_drain_never_flushes_bare_on_the_async_worker` (source scan) — the
   `offloaded` count is inverted from 1 to **0**, and a new assertion requires
   `return blocking_flush(|| ingest.flush());` to still EXIST so that deleting
   the offload branch entirely also fails. The `wrapped` equality moves 1 → 2:
   both arms of the one helper now wrap. Bite-proven — restoring the bare
   return fails it by name.
2. `the_offloaded_path_still_flushes_the_inline_depth_sink` (behavioural, NEW) —
   builds an offloaded `LiveIngest` with an inline-depth sink holding 10 rows
   and zero tick rows, flushes, and asserts the depth sink's `dropped_rows`
   reached 10. `DepthWriter::for_test` has no sender, so a discard is proof the
   flush was attempted.

   Bite-proven by moving the inline-depth flush BELOW the `pending_rows == 0`
   early return: **every source scan still passed and only this test failed.**
   That is the whole reason it exists — the defect was in a *claim about
   behaviour*, and a text match cannot check a claim.

### Rollback

Revert the one-line wrap plus the two guard edits. The bare form is what
shipped before; reverting restores a known (bad) state, not an unknown one.

### Observability

None added, deliberately. There is no metric that distinguishes "the drain is
blocked in HTTP" from "the drain has nothing to do" — both look like an idle
task. The existing signals already cover the consequences: ring-full refusals,
`tv_ticks_dropped_total`, the WAL drop counter revived in Item 25, and the
depth flush-failure counter. Adding a fourth that fires only in the same
window would be noise.

### Honest envelope

Removes ONE blocking call from ONE task. It does not make the flush faster, does
not reduce the ~94:1 write amplification, and does not stop QuestDB stalling —
it stops a QuestDB stall from also costing ticks and a socket. `flush_depth`
(the dedicated depth pools) was already wrapped and is unchanged.

**The reusable lesson, recorded because it outlives the fix:** the source-scan
guard DID hold the line it was built to hold, and was then walked around by an
exception written in good faith from an incomplete reading of the callee. A
guard is only as strong as the claim its exception rests on, and this one rested
on a claim nobody re-derived from `LiveIngest::flush`.

---

## Item 31 — the drain's own backlog was measured 5,000 times a second and thrown away

### Design

`run_frame_drain` derives `queued_nanos` — how long a frame sat in the ring
before the fold reached it — on EVERY frame, uses it once to back-date
`received_at_nanos`, and drops it.

That number is the lane's best leading indicator and nothing has ever read it.
Every other ring signal is a post-mortem count: `tv_dhan_ws_ring_full_total`
and `tv_dhan_ws_frame_refused_total` move only once frames have already been
turned away. Dwell rises while there is still headroom.

**Fix:** `record_ring_dwell(queued_nanos)` keeps the maximum in a module
`AtomicI64`; `publish_fold_depth` drains it to
`tv_dhan_feed_ring_dwell_max_ms` on the existing periodic path. EMF-selected
(80 names now, +$0.30/mo, priced in the count ratchet) and charted.

**Rejected alternative — shipping `tv_dhan_ws_lag_ms` instead.** That measures
the same axis from the vendor's side, and two things rule it out. It is an
EXPLICIT exclusion in `EMF-METRIC-SELECTOR-NOTES.md` ("latency histograms …
answer 'how much', not 'what broke'"), and a histogram ships ~12 bucket series
per dimension — so the per-connection form the 2026-08-14 noise-lock
authorization priced at $4.80/mo is closer to an order of magnitude more. That
discrepancy is recorded rather than spent silently. Dhan's delivery lag is also
not ours to fix; our own backlog is.

### Edge cases

- **Quiet window, no frames** — publishes 0, not the previous window's value.
  Carrying it forward would make an idle lane look identical to a stalled one.
- **Zero dwell** (the healthy steady state) — recordable, and must not displace
  a real sample. Pinned.
- **Clock steps** — none possible: `frame.received_at.elapsed()` is monotonic.
- **`i64` overflow** — `try_from(...).unwrap_or(i64::MAX)` was already there.

### Failure modes

Two, and both are invisible in the code's shape, which is why the tests are
behavioural rather than source scans:

1. **Last-value-wins instead of max.** One 8-second stall followed by three
   1 ms frames would publish 2 ms — the signal says "fine" at the exact moment
   the drain has stopped. Bite-proven: `fetch_max` → `store` fails two tests.
2. **Sticky maximum.** A stall from hours ago keeps the chart red forever, and
   a permanently-red chart costs more than not publishing. Bite-proven:
   `swap(0)` → `load` fails two tests.

### Test plan

`crates/app/tests/ring_dwell_gauge_guard.rs`, 6 tests: worst-sample-survives,
reset-on-read, quiet-window-zero, unit direction (ns→ms, not the reverse — the
error that would read as a 17-day stall and look plausible because "big number,
bad situation" is self-consistent), zeros-do-not-poison, and one source check
that the recorder is wired into the drain and the gauge is actually SET —
which earns its place because the defect being fixed IS "computed and never
published", so an arithmetic-only suite would pass on the broken code.

Serialised behind an explicit mutex, not `--test-threads=1`: the latter is a
property of how the suite is invoked, not one anything can rely on.

### Rollback

Delete the two helper calls and the selector/dashboard entries. The lane
behaves exactly as before — nothing reads this value.

### Observability

The point of the item. `tv_dhan_feed_ring_dwell_max_ms`, EMF-selected, charted
as "How far behind the drain is (worst frame wait, ms)", `stat = Maximum`.

**Deliberately UNALARMED.** No threshold is defensible yet: the value has never
been observed, because it has never been published. Picking a number now
invents it and then trains the operator to ignore the alarm built on it. Chart
first; threshold when the chart has a baseline.

### Honest envelope

Publishes a number that already existed. It does not make the drain faster, does
not enlarge the ring, and does not prevent a stall — it makes one visible before
it costs ticks instead of after. Cost on the hot path is one uncontended atomic
`fetch_max` per frame (single writer, `Relaxed`), no allocation and no registry
lookup; the gauge write is on the existing periodic path, never per frame.

---

## Item 32 — the deaf socket: one connection stops delivering and nothing notices

### Design

A socket that keeps answering pings but stops delivering data is invisible to
every mechanism the lane has, and each miss is structural rather than an
oversight:

| Mechanism | Why it cannot see this |
|---|---|
| Idle watchdog (27 s) | governs SILENCE on the wire; a ponging socket is not silent |
| Reconnect family (`reconnect_total`, `dial_failed_total`, `subscribe_failed_total`) | stays flat — the defining property of a deaf socket is that **nothing about it is retrying**. This is why "alarm the reconnect counters", the recommendation on record, cannot work |
| Lane `tv_dhan_feed_last_tick_age_secs` | reads ~1 s throughout: fifteen of sixteen sockets are fine |
| `tv_dhan_ws_alive_connections` | counts sockets DIALED, not sockets DELIVERING |

**Fix:** a fixed `[AtomicI64; MAX_TOTAL_DHAN_CONNECTIONS]` of last-delivery
millis, stamped per frame that actually produced output, published as ONE gauge
`tv_dhan_ws_worst_conn_tick_age_secs` = the worst age across connections that
have ever delivered.

**One series, not sixteen.** Per-connection is ~$4.80/mo by the 2026-08-14
noise-lock figure to answer a yes/no question; the worst-age form answers it
with identical detection power for ~$0.30/mo. Per-connection attribution stays
on `/metrics`, which is where a human triaging looks. Charted BESIDE the lane
gauge, because the difference between them is the diagnosis: both low = healthy,
this one climbing alone = one deaf socket, both climbing = the whole feed.

### Edge cases

- **Never-ticked slot** — reads `0` and is SKIPPED, not treated as infinitely
  stale. An unused slot would peg the gauge at "broken" forever, and a
  legitimately quiet depth-200 subscription on one illiquid contract is not a
  fault. That case already has `tv_dhan_feed_instruments_never_ticked` +
  RISK-GAP-03.
- **No connection has ticked at all** — publishes `-1`, never `0`. Zero would
  read as "every socket ticked this instant" — the most reassuring value
  available — at the moment we know least.
- **Clock steps backwards** (NTP) — `saturating_sub(...).max(0)`, so it reads 0
  age rather than wrapping into a huge one at the moment logs are hardest to
  read.
- **Out-of-range connection index** — dropped. Writing the wrong slot would
  report the wrong socket healthy, which is worse than not writing.
- **Frames that arrive but produce nothing** — do NOT stamp. A socket returning
  unparseable frames is not delivering market data, and stamping on arrival
  would report it healthy: the exact false-OK this detector removes.

### Failure modes

1. **Min instead of max** — reports the most recent tick anywhere, i.e. perfect
   health with a socket dead. Bite-proven: `w.max(age)` → `w.min(age)` fails 2
   tests.
2. **Depth stamp forgotten** — ten of sixteen sockets read never-ticked, are
   excluded, and the gauge looks healthy *because* of the exclusion rule that
   exists to prevent false-OKs. Bite-proven: deleting it fails the wiring test.

### Test plan

`crates/app/tests/deaf_socket_gauge_guard.rs`, 6 tests, all behavioural except
one wiring scan: one-deaf-among-healthy, all-healthy-reads-zero, recovery
lets it fall, backwards clock clamps, out-of-range dropped, and both stamp
sites + the publish are wired. Fixed `NOW_MS`, so nothing depends on the wall
clock.

### Rollback

Delete the static, the two stamp calls, the publish, the selector entry and the
widget. Nothing else reads any of them.

### Observability

`tv_dhan_ws_worst_conn_tick_age_secs`, EMF-selected (81 names, +$0.30/mo,
priced in the count ratchet), charted as "Deaf socket check" against the lane
gauge for contrast.

**NOT alarmed — and this one is a deliberate deferral, not a missing baseline.**
A threshold is defensible here (600 s, matching the lane-level no-ticks alarm —
the same question scoped to one socket). What stops it is money: the maximal
month already projects above the line where an AWS budget action STOPS the
trading box. Adding a pager is a spending decision the operator takes knowingly,
not one an executor slips in. Charted now so the signal exists the moment he
says yes.

### Honest envelope

Makes a deaf socket VISIBLE; does not detect it automatically, because nothing
pages on it yet. Detection latency is the 30 s scan cadence plus whatever
threshold is eventually set. It cannot distinguish "the socket is deaf" from
"every instrument on this socket genuinely stopped trading" — an operator reads
it against the lane gauge and the never-ticked count, which is why they share a
screen. Hot-path cost is one relaxed atomic store per productive frame.

---

## Item 33 — `late_count` counts data loss and had zero production readers

### Design

`ConsumeStats.late_count` is the number of timeframes that DISCARDED a tick as
too late to place (`ConsumeOutcome::DiscardLate`). A discarded tick means the
bar it should have contributed to is missing a trade — a wrong bar.

It was computed for all 24 timeframes on every tick and **read by nothing**.
Every production consumer reads `sealed_count` and `amended_count`, and
`IngestOutcome::Folded` carries only those two, so the value existed inside one
function call and died there.

**Fix:** increment `tv_candle_tick_discarded_late_total` at the discard arm,
through the pre-resolved `fold_counters()` handle.

**Its own metric name, not a `reason` label on
`tv_aggregator_tick_refused_total`.** That family means "the whole tick was
refused and NOTHING was folded" — a different and stronger statement. A tick
counted here DID fold into other timeframes; merging them would make both
numbers unreadable.

### Edge cases

- **Per (tick, timeframe), not per tick** — one tick can be placeable in the
  1-day bar and hopeless for the 1-second one. Collapsing to a per-tick number
  would hide which frames are losing data.
- **Interaction with Item 27** — the stale-trading-day gate now refuses
  previous-day ticks BEFORE the timeframe loop, so what reaches this arm is
  today's ticks that are late for a given frame: a smaller, more interesting
  population than it would have been last week.

### Failure modes

The counter increments inside the 24-timeframe loop on the per-tick path. A
bare `metrics::counter!` there costs a sharded-registry lookup 24× per tick —
the exact defect `fold_counters.rs` was created to remove. Pinned: the test
asserts the handle is used AND that no bare macro appears in the arm.

### Test plan

`the_late_discard_is_counted_and_not_merely_tallied_into_a_dropped_struct` in
`stale_trading_day_gate_guard.rs`. Bite-proven — deleting the increment fails
it and nothing else. `dhat_multi_tf_fold` re-run: still zero allocations.

Two scanner defects were fixed in writing it, both of the kind this session has
been correcting elsewhere:

1. `split("#[cfg(test)]").next()` truncated ~500 lines ABOVE the arm, because
   this file carries test-only helpers inside production code. Split at the
   test MODULE boundary instead.
2. A fixed byte window missed the increment once a comment was added — the same
   proximity brittleness rewritten out of two guards earlier today. Replaced
   with brace matching on the arm's actual block, which is the real boundary.

### Rollback

Delete the increment and the handle. Nothing else reads either.

### Observability

`tv_candle_tick_discarded_late_total` on `/metrics`.

**Deliberately NOT EMF-selected.** Two names were added to the selector today
(+$0.60/mo) and the maximal month already projects above the AWS budget
action line. This one is a DATA-QUALITY signal rather than an outage signal —
it says bars may be incomplete, not that the feed is down — so it is the
right one to leave on `/metrics` when something has to be. Shipping it is a
one-line change plus a priced count bump whenever the operator wants it.

### Honest envelope

Makes the loss COUNTABLE, not smaller. It does not recover a discarded tick and
does not change which ticks are discarded — the amend/discard boundary is
untouched. Nothing pages on it, so it is only visible to someone already
looking at `/metrics`.

---

## Item 34 — we shipped the ring's denominator and withheld its numerator

### Design

The frame ring is bounded twice: by frame COUNT (65,536) and by BYTES
(`RingByteBudget`). `tv_dhan_feed_ring_max_bytes` — the CAPACITY — has been
EMF-selected since 2026-08-15. The OCCUPANCY was not published at all:
`RingByteBudget::resident()` existed with call sites only in its own unit
tests, the fifth instance in this lane of code that exists, is tested, and is
never invoked.

**Fix:** `tv_dhan_feed_ring_resident_pct` — worst of the two pools —
published on the same 30 s tick as the dwell and deaf-socket gauges. Per-pool
detail goes to an UNSELECTED name, so `/metrics` keeps the attribution and
CloudWatch pays for one series.

**Percent, not bytes.** The two budgets are sized 3:1, so raw bytes are not
comparable between them and a raw gauge would be dominated by whichever pool
is larger regardless of which is in trouble.

**It pairs with the dwell gauge, and the pair is the diagnosis:**

| dwell | fill | reading |
|---|---|---|
| flat | flat | healthy |
| up | up | the drain cannot keep up |
| flat | up | large frames, not a slow drain |
| up | flat | slow drain on small frames |

Neither number says that alone, which is why they are charted side by side.

### Edge cases

- **Zero capacity** — returns 0.0 rather than dividing. Unreachable in
  production (both caps derive from a non-zero host figure), but a NaN on a
  gauge is silently unchartable, and "the chart went blank" is worse than "the
  chart read zero": blank looks like the app died and sends an operator to
  diagnose the wrong thing mid-incident.
- **Over capacity** — clamps to 100. `RingByteBudget` uses a CAS loop
  specifically so `resident` cannot exceed `cap`, so this should be
  unreachable; a gauge reading 340% would look like a unit bug and be
  dismissed at the exact moment the ring is in trouble.
- **Both pools idle** — reads 0, which is correct and not a stale value.

### Failure modes

**Reporting the average instead of the worst.** With main-feed at 1% and depth
at 95% the average reads 48% — healthy-looking — while one pool is about to
refuse frames. Bite-proven: `main_pct.max(depth_pct)` → `(a+b)/2.0` fails the
wiring test.

### Test plan

`crates/app/tests/ring_fill_gauge_guard.rs`, 5 tests: empty/full/quarter, zero
capacity is finite and zero, over-capacity clamps, worst-of-two surfaces, and
the publish reads the LIVE budgets (publishing `cap()` as occupancy would give
a permanently-100% chart that reads as a crisis and is a bug).

### Rollback

Delete the const, the helper, the three gauge writes, the selector entry and
the widget.

### Observability

`tv_dhan_feed_ring_resident_pct` EMF-selected (82 names, +$0.30/mo) and charted
as "Ring fill (% of byte budget, worst pool)" beside the dwell chart.
`tv_dhan_feed_ring_resident_pct_by_pool{pool}` on `/metrics`, unselected.

**NOT alarmed.** The ring already pages on the after-the-fact case
(`tv_dhan_ws_ring_full_total`, `tv_dhan_ws_ring_bytes_full_total`), so the loss
is covered today; this is its leading edge, and a leading-edge threshold needs
a baseline that does not exist yet.

**THREE names added today (~$0.90/mo) and that is the stopping point.** The
maximal month already projects above the line where an AWS budget action stops
the trading box. Everything else found today stays on `/metrics`.

### Honest envelope

Publishes a number that already existed, on a path that already ran. It does
not enlarge the ring, speed the drain, or prevent a refusal. Sampling is the
30 s tick, so a sub-30-second fill spike is invisible — the after-the-fact
counters remain the only signal for that, and they page.

---

## Item 35 — the cross-verification runs 94% blind and cannot say why

**Status:** DONE (2026-08-26)

### Design

The 15:41 IST comparator against Dhan's own REST tape is, by this repository's
own doctrine, the **only ground truth the revived feed has** — the India feed
carries no sequence number and no snapshot-on-subscribe, so packet loss is
undetectable at the protocol level and nothing inside our pipeline can prove a
tick never arrived.

Read from the live box, two consecutive sessions:

| session | targets | `rest_failures` | actually compared | diverged | typical gap |
|---|---:|---:|---:|---:|---:|
| 2026-08-24 | 864 | **814** | ~50 | 61 | 35 paise |
| 2026-08-25 | 865 | **815** | ~50 | 23 | 25 paise |

**94% of the only ground truth fails every day.** Where it does compare, we
agree with Dhan closely — 25 paise on prices in the thousands. The comparison
is not failing; it is barely running.

And the reason was **unknowable from the box**. `fetch_rest_side_for_target`
has always built a precise reason string for every failure mode; its only
caller matched `Err(_)`, counted `rest_failures += 1`, and dropped it. A count
without a cause is a symptom that cannot be treated, and it had gone untreated
for at least two sessions.

Two changes:

1. **Typed failure kinds** (`XverifyFetchFailureKind`, a closed 7-variant set:
   `no_candles`, `rate_limited`, `http_4xx`, `http_5xx`, `timeout`,
   `transport`, `other`), tallied per run into a fixed-width
   `RestFailureBreakdown` and logged on the same line as the verdict, plus a
   bounded 3-entry sample of real messages. HTTP status is classified from the
   REAL `StatusCode` via a pure, directly-tested `classify_http_status` — never
   a substring scan of a rendered message.
2. **Pacing.** The loop issued ~865 requests back-to-back with no pacing at all
   against a documented 5-per-second vendor budget, while both sibling REST
   legs have gone through the shared self-tuning limiter since 2026-07-14. This
   leg was the odd one out. A new `fetch_rest_side_paced` wrapper acquires the
   shared limiter and feeds the self-tuner on a real 429.

### Edge cases

- **429 is a client error**, so a naive `is_client_error()` branch first would
  swallow it into `http_4xx` and hide the one reason that is directly
  actionable. Order is asserted, and the test states why.
- **`no_candles` is ambiguous by construction** — a wrong instrument label and
  a genuinely untraded instrument produce the identical 2xx-with-no-candle
  response. The count is what separates them: 815 of 865 is not 865 untraded
  instruments. Documented at the variant rather than papered over.
- **Empty breakdown** renders `none`, so a clean run stays short.
- **Zero-count kinds are omitted**, dominant kind first, so one line answers
  "why" at a glance.

### Failure modes

**Pacing could push the run past its 600 s budget.** At the limiter's 3 rps
that is ~288 s of pacing floor for 865 targets, inside the budget; the loop
already breaks on budget with `budget_elapsed = true`, which is honest rather
than silent. Watched, not assumed.

**The two changes could confound each other** — they do not. The breakdown
labels the outcome that actually occurred, so if tomorrow reports
`rate_limited=0` then pacing was never the cause and the dominant label names
the real one.

### Test plan

5 tests in `dhan_live_crossverify.rs`: distinct stable labels; a real 429
classifies as `rate_limited` and not as a generic 4xx; the breakdown total
always equals the failure count; the summary leads with the dominant reason and
omits zeroes; and the regression pin that the loop tallies the reason, never
discards it, and goes through the paced wrapper.

**Bite-proven both directions:** reverting the paced call fails the pin;
deleting the tally line fails the pin.

The pin scans the **production half only** — `include_str!` reads this file
including its own tests, whose assertion strings contain the banned pattern, so
the first run failed on itself. The test-module marker is assembled from
fragments rather than written whole, because spelling it out put a second copy
in the file and split the source in the wrong place — the second run failed on
that. Both are recorded at the test: a self-scanning guard fails OPEN, and this
one would have passed while reading nothing.

### Rollback

Revert to `Err(_) => rest_failures += 1` and drop the wrapper. That restores a
comparator that reports 94% blindness without a cause.

### Observability

**No new metric name, no new EMF selector entry, no cost.** The breakdown rides
the existing `cross-verification finished` line as `rest_failure_reasons`, plus
one `warn!` carrying the reasons and a 3-entry sample. Deliberate: EMF folds
labels to `{host}` by summing, so a labelled counter would surface only the
total — which the log already carries — while costing ~$0.30/mo, and the
maximal month already projects above the line where a budget action stops the
trading box.

### Honest envelope

This makes the blindness **explainable**, not smaller. Tomorrow's run names the
dominant cause; fixing that cause is the next change, not this one. Pacing is
hygiene on a documented budget, **not a claimed cure** — whether rate limiting
is what blinded the comparator is exactly what the breakdown answers.

Nothing here improves agreement with Dhan. Where the comparison does run we
already agree to ~25 paise; the problem was never the matching, it was the
coverage.

---

## Item 36 — volume was never compared, and the operator asked about volume

**Status:** DONE (2026-08-26)

### Design

Operator, this morning, verbatim (typos preserved):

> "precisley meanwhiel exactlt at 3.40 pm you need to pull the entiree 850 or
> 860 spots entikeey rgith dude that too one an donly for one mintue alone
> especiallu to do the cross verification rigth dude **whetehr our entire
> system ophlc vlolume is entilrey amtchign for evrythginr ight dude**"

He asked whether OHLC **and volume** match. Checked in source: `PaiseBar::fields()`
returns exactly four entries — open, high, low, close. **Volume was never compared.**
It rode along on every finding row as `live_volume` / `rest_volume` and no
divergence was ever raised on it, so "does our volume agree with the exchange's?"
had no answer anywhere in the system.

That gap is not academic. A live volume defect measured 2026-08-24 had the
intraday frames at **~9.2× the day bar**, with 6,088 instruments disagreeing with
their own minute sums. The one comparison that could have caught it was not
looking.

**Volume is reported as a CAPTURE PERCENTAGE, not a pass/fail.** Our live feed is
a conflated ~1/sec sample; the vendor's REST tape is the full record. Under-capture
is STRUCTURAL and expected, so a strict equality check would flag nearly every cell
and drown the price signal beside it. Four figures: cells with a usable volume,
exact matches, the median capture, the 5th percentile, and the worst single cell.

It deliberately does not feed `cells_diverged` or the outcome. A threshold needs a
baseline and no baseline exists — this measurement is what creates one. Identical
discipline to the price side, which quantifies its noise rather than asserting a
limit.

**Also in this item: the match rate is now computed, not left as homework.**
`minutes_compared` and `cells_diverged` were both already on the verdict line, so
the rate was always derivable with one multiplication — and nobody did it, for two
sessions, while the run sat at 0.09% coverage. A number that needs arithmetic
before it means anything is a number that gets skipped.

### Edge cases

- **The vendor reports zero volume for a minute.** Skipped, not scored 0%. Their
  volume is the denominator; a zero one means they saw no trades, which says
  nothing about our capture, and counting it would drag the median toward "we
  missed everything" when the truth is "there was nothing to miss."
- **Over-capture (above 100%) is reported, never clamped.** Reading above the
  vendor's own tape cannot happen honestly — it is the signature of the
  double-counting defect measured on 2026-08-24. Clamping would erase the only
  evidence of it.
- **Integer arithmetic throughout.** A float ratio would be the one place in this
  module comparing by epsilon, which its own header forbids.

### Failure modes

**A volume gap misread as a price divergence.** Pinned: prices agreeing exactly
while volume differs 1-vs-9,999 must leave `cells_diverged` at zero and the
verdict a pass.

**The bad tail hidden by the median.** Caught by a test that FAILED as first
written. `percentile` here is linear-rank, so at n=20 the 5th percentile lands on
the second-smallest value and one catastrophic minute never reaches it. That
failure is what added `volume_capture_min_pct` — p05 describes the distribution,
the minimum describes the worst thing that actually happened. The price side
reports `max` for the same reason; volume's bad direction is simply down.

### Test plan

6 tests: volume is compared at all; a volume disagreement never becomes a price
divergence and never flips the verdict; a vendor-untraded minute is skipped rather
than scored zero; a perfect capture reads 100 including its tail; the median hides
one bad cell in twenty and the minimum does not; over-capture survives to the
report at 920%.

### Rollback

Delete the four fields, the loop block, and the log fields. That restores a
comparison that checks four of five columns and never says so.

### Observability

**No new metric name, no selector entry, no cost.** Five fields on the existing
verdict line.

### Honest envelope

This MEASURES volume agreement; it does not enforce it. No threshold is set,
because none can be honest before a baseline exists — tomorrow's run produces the
first one. It also cannot separate "we missed trades" from "the vendor's tape
disagrees with itself", and it inherits the coverage limit of Item 35: measuring
volume across 295 price-minutes says very little until that number grows.

### Rider — the FIFTH fixed-window guard

`the_cross_verify_verdict_is_logged_as_fields_not_a_debug_dump` scanned a FIXED
2,000 bytes back from its marker. Adding five fields to the emit made the list
outgrow the window and the guard failed — not because a field was missing, but
because the code got longer. It failed CLOSED (blocking a correct change), which
is the better direction, but the shape is the same one corrected four times
already in this branch. It now walks back to the enclosing macro opening, which is
a real boundary and cannot drift. Bite-proven: deleting the field it pins fails it.

---

## Item 37 — a fixed sweep order starved the tail of the universe forever

**Status:** DONE (2026-08-26)

### Design

Operator, verbatim: *"ensure to pull evrythign using incrmental or decrmental
aprpoach ddude okay?"*

The fetch loop walked its target list in FIXED order and `break`s on a hard
wall-clock budget. Fixed order plus truncation is not "we reach fewer
instruments" — it is **"we never reach the tail"**, identically, every day,
while the report shows a respectable coverage percentage and nothing says
which instruments were skipped.

Today's pacing change (Item 35) is what makes it bite. Measured:

| limiter rate | per-answer | 868 targets need | vs 600 s budget |
|---|---|---:|---|
| 4 rps | 0.2 s | 391 s | covers all |
| 3 rps | 0.4 s | 637 s | **truncates ~6%** |
| 2 rps | 0.4 s | 781 s | **truncates ~23%** |

At the limiter's floor a specific 23% of the universe would go permanently
unverified — in the one check that exists to prove nothing was lost.

**The fix is a rotating start, and it is deliberately STATELESS.** A persisted
cursor resumes exactly and is also a file to lose, corrupt, or have differ
between a laptop and the box — a new failure mode inside the only loss
detector. The trading date is state everyone already agrees on.

**The stride is the guarantee.** Consecutive runs cover `[k·s, k·s + c)`;
their union is the whole list exactly when `c >= s`. At `s = ceil(n/3)` any run
reaching a third closes the universe in three days, and the measured worst case
(77%) closes it in two.

### Edge cases

- **Empty target list** — returns 0 rather than dividing by zero. A wrong start
  is survivable; a panic in the only loss detector takes the day's
  verification with it.
- **`n = 1`** — `max(1)` keeps the stride positive.
- **Year boundary** — days-from-epoch, not day-of-year. Day-of-year snaps to 0
  every January and re-starves whatever December was working through.
- **A 25,000 universe** — the day number (~739,000) is reduced modulo `n`
  BEFORE multiplying by the stride, so the product cannot overflow a 32-bit
  `usize`.

### Failure modes

**The off-by-one a test caught.** `n / 3` rounds DOWN: at n=868 the stride is
289, three of which is 867 — leaving exactly one index that three strides never
reach, starved forever. `div_ceil` closes it. That is the same defect this
rotation exists to kill, one index wide instead of two hundred.

**Rotation that is not a permutation.** If the wrap skipped or repeated an
index, an un-truncated run would compare one instrument twice and another
never. Pinned for n ∈ {1, 2, 7, 868}.

### Test plan

7 tests: empty list does not divide by zero; consecutive days differ; three
days at exactly one-third coverage reach everything; the measured 77% closes in
two days; a full sweep is a permutation; a year boundary does not reset;
a 25,000 universe does not overflow.

**Bite-proven:** pinning the start back to 0 fails **5** of them.

### Rollback

Delete `rotation_start` and iterate `targets` directly. That restores a sweep
that starves the same tail every day.

### Observability

**No new metric name and no cost.** The coverage fraction the guarantee depends
on already rides the verdict line as `targets` and the compared count.

### Honest envelope

This converts permanent starvation into eventual coverage. It does **not** make
any single day complete — a truncated run still leaves that day's tail
unverified, and only the next runs close it. The three-day guarantee holds only
while coverage stays above a third; if it falls below, the rotation no longer
covers everything, and the reported fraction is what would show it. Nothing
here reduces the 815 fetch failures — that is Item 35's diagnosis, due
tomorrow.

---

## Item 38 — the vendor's own tape was fetched, judged, and thrown away

**Status:** DONE (2026-08-26)

### Design

Operator, verbatim: *"see to save this also you need a separte tabel right
becuase onl yfrom there ou can do the cross evrificationa dn even stroe the
hisotircla data right even for manaul verificaiton also right dude am ii rght
dude okay?"*

He is right, and checking confirmed it. `fetch_rest_side_for_target` pulls the
exchange's own minute candles, `compare_day` judges them, and the bars are
**dropped**. Only the cells that DISAGREED survive, on the cell-audit table.
Three consequences, all of which he named:

1. **No manual verification.** "What did Dhan say for this instrument at
   09:16?" is unanswerable unless that minute happened to diverge.
2. **No re-verification without re-fetching.** Re-running the comparison means
   ~868 more requests against a rate-limited vendor budget — so in practice it
   was never re-run.
3. **No other analysis.** The vendor's own record did not exist on disk in any
   form.

Storing the pull turns the comparison into a pure function over two stored
tables: fetch once, compare as often as you like, offline.

**⚠ The trap that forced a NEW table.** `rest_spot_1m` already exists with
exactly the right shape — and its DEDUP key is
`(ts, security_id, exchange_segment, feed)`, which **omits its own `source`
column**. Writing this 868-instrument sweep there would have silently
overwritten the per-minute leg's index rows, with no error anywhere. That is
the third silent-overwrite risk found today. The new table carries `source`
**in the key from its first line**, so the same collision cannot be introduced
later by a second writer.

### Edge cases

- **The designated timestamp is the MINUTE, not the run time.** A run-level
  stamp would file every candle of the day under one instant and make "what
  was the 09:16 price" unanswerable — the exact question the table was added
  for.
- **`fetched_at` is stamped PER TARGET.** The loop runs up to ten minutes; one
  run-level stamp would claim the last instrument was fetched at the same
  instant as the first, destroying the only number that says how stale the
  vendor's record was when read.
- **`fetched_at` uses the existing IST helper**, not a second clock. The
  offset rule is already applied there, and a private copy is how the
  timestamp class that started this whole session gets re-created.
- **`instrument` is stored verbatim.** A vendor "no candles" answer is
  identical for a mislabelled instrument and a genuinely untraded one; without
  the label there is nothing to check against the master afterwards.
- **Memory.** The bars already exist in memory — carrying them out is a move,
  not a copy. At full coverage the tape is ~333,000 rows ≈ 37 MB, and today's
  coverage makes it far smaller.

### Failure modes

**A future "simplification" pointing this writer at the sibling table** —
pinned by a test asserting the table names differ, with the collision spelled
out at the assertion.

**A half-registered table.** A table whose CREATE lands without a DEDUP ENABLE
auto-creates on first write with **no dedup at all** — a silent duplicate-row
window. The DDL test now checks per table (one CREATE, one DEDUP ENABLE, one
ALTER per declared column) rather than one total, so a half-registered table
fails loudly instead of sliding under a sum that still adds up.

**Silent reversion to fetch-and-discard** — pinned by a source scan of the
persist function.

### Test plan

7 storage tests: `source` is in the key; `security_id` is paired with
`segment` and `feed` is in-key; the designated timestamp is the minute; the
asked-with label survives; the DDL is registered with dedup enabled; two
identical rows produce byte-identical wire output; the table is not the
sibling table.

2 app wiring tests: the tape is persisted and partial writes counted; the
stamp is taken inside the fetch loop, before the rows that carry it.

**Bite-proven:** dropping `source` from the key fails 2; disabling the persist
call fails 2.

### Rollback

Delete the table constants, DDL, writer method, the `rest_tape` field, and the
persist loop. That restores a comparison whose raw input exists nowhere.

### Observability

**No new metric name and no cost.** Tape rows count into the existing persist
counter; partial tape writes ride the existing partial-write error line.

### Honest envelope

This stores what we fetched. It does **not** increase what we fetch — the
tape is exactly as complete as the run that produced it, so at today's 0.09%
coverage the stored tape is equally sparse. It also stores only the VENDOR
side; our own candles already live in the live tables, and joining them is the
reader's job.

**Retention — the gap this envelope named, now closed (2026-08-26, same
day).** The first draft of this section said "no archival policy is registered
for it here, so it grows with the hot window like its siblings", and
`partition_retention_coverage_guard` failed the build on exactly that: a new
storage table constant with no retention decision. `dhan_rest_1m_tape` is now
in `DAY_PARTITIONED_TABLES`, swept by the same detach→archive pass as
`dhan_live_crossverify_cell_audit` and `dhan_live_crossverify_daily`.

It is registered with its real volume rather than the word "trivial", because
it is not: the cell table is bounded by DIVERGENCES and is normally near-empty,
this one is bounded by INSTRUMENTS. One rotation third (~290 of ~865 spots) ×
375 session minutes ≈ **108K rows/day** at full success, ~30 MB/day at the
measured ~272 B/row, so **~2.7 GB across the 90-day hot window** — the largest
per-day writer in that list. At today's measured 50-successful-fetches ceiling
it is ~19K rows/day, so the number will GROW as the fetch failures are fixed,
which is the opposite of the usual direction and worth stating before it
surprises someone.

---

## Item 39 — the whole day's cross-verification was discarded, and 99% of it was never data

**Status:** DONE (2026-08-26)

### Design

Found by reading the live logs rather than waiting for tomorrow's run.

Today's 15:41 IST run **failed to persist at all**:

> `dhan_live_crossverify ILP flush failed — 764003 pending row(s) discarded`
> `Buffer size of 207965278 exceeds maximum configured allowed size of 104857600`

**The entire day's comparison was lost** — in the one check that exists to
prove nothing else was.

**Why 764,003 rows.** The verdict line carries `instruments: 8144` beside
`targets: 865`. The live universe holds every option and future contract; the
REST leg fetches ~865 spots. `compare_day` built its key set from live ∪ rest
with **no scope filter**, so every minute of the ~7,279 never-requested
instruments was recorded as `missing_rest` — *"the vendor is missing this"* —
for data nobody asked the vendor for.

**757,273 of 764,003 findings (99.1%) were that.**

Two fixes, deliberately independent:

1. **The scope filter** (root cause). `compare_day_in_scope` takes the set the
   run actually requested; a live instrument outside it produces no findings
   and is not counted in the reported universe.
2. **Batched flushing** (floor). 20,000 rows per flush — not a round number:
   the breached buffer averaged ~272 bytes/row, so a batch lands near 5 MB
   against a 100 MB ceiling. A future legitimately-large day now loses at most
   one batch instead of everything.

Two mechanisms because *"the row count can never grow again"* is precisely the
assumption that produced the ceiling breach.

### Edge cases

- **An empty scope disables the filter**, so every existing caller and test
  means exactly what it did before.
- **The scope key pairs `security_id` with `segment`** (I-P1-11). The same
  numeric id in another segment is a different instrument; matching on the id
  alone would admit one we never asked for and re-create the defect one
  instrument at a time.
- **A requested instrument the vendor did not serve is STILL `missing_rest`.**
  The filter must not silence the signal it sits beside — that is the one case
  the category exists to report.
- **The skip happens BEFORE the instrument count.** Counting it would report a
  universe far larger than the one verified, which is how `8,144 instruments`
  sat in the verdict beside 865 targets and read as coverage.

### Failure modes

**The filter hiding real loss** — pinned by the asked-for-and-not-served test.

**A silent regression to the unscoped comparison** — bite-proven: removing the
filter fails 5 tests.

### Test plan

6 tests: a never-requested instrument produces no findings; it is not counted
in the universe; a requested-but-unserved one still reports missing; an empty
scope filters nothing; the key pairs id with segment; and the measured prod
shape (200 live, 3 targeted) collapses to 3 findings rather than 200.

### Rider — my own guard fired twice, correctly

The single-test-marker guard written this morning caught the helper placement
**twice**: once for a test-only attribute in production code, and once for a
comment that merely NAMED that attribute. Both times it was right. That is the
third instance today of a source scanner unable to tell quoting from doing, and
the helper now lives inside the test module with nothing spelling the attribute
out.

### Rollback

Pass an empty scope set and drop the batch flush. That restores a comparison
that reports 99% noise and loses the day when it grows.

### Observability

**No new metric name and no cost.** `batch_errors` rides the existing
partial-write error line.

### Honest envelope

This makes the day's comparison **persistable and its findings meaningful**. It
does **not** improve coverage: the same ~50 instruments still succeed. It does
not reduce the 815 fetch failures. And it changes what `missing_rest` MEANS —
historical rows carry the old, inflated definition, so any trend across
2026-08-26 is a definition change, not a signal.

## Item 25 — the two percentage columns exist on every candle and have never held a value (2026-08-26)

**Operator instruction (2026-08-26, verbatim, typos preserved):**

> "i believe pre open percetage cjmaeh and percnetahe change both of them not
> needed for ticks dude it is oen and only need for seocnds an dminutes dude
> okay? see emanwhiel hwow ill you ensure that evn seconds level also will
> hodl thes eprecise ohlcv and even minute laso ohlcv dude okay? can you lsit
> i tour dud eoaky?"

He is narrowing an earlier request: the percentage change and the pre-open
percentage change belong on the **seconds and minutes frames**, not on the raw
tick table. That is exactly where the columns already are — and that is what
makes the finding below worth an item rather than a shrug.

### The finding (MEASURED live, not inferred)

`candles_<tf>` has carried `open_pct`, `open_gap_pct`, `change_pct` and
`close_pct_from_prev_day` since the Wave-5 seal-column work. Read live on the
production box, 26 Aug 2026, market session only:

| Frame | Bars stored today | With `open_pct` ≠ 0 | With `open_gap_pct` ≠ 0 |
|---|---:|---:|---:|
| `candles_1s` | 9,038,254 | **0** | **0** |
| `candles_5s` | 4,236,485 | **0** | **0** |
| `candles_15s` | 2,399,970 | **0** | **0** |
| `candles_1m` | 1,065,763 | **0** | **0** |
| `candles_3m` | 520,649 | **0** | **0** |
| `candles_15m` | 148,183 | **0** | **0** |

17.4 million bars, six frames, not one value. The cause is not a plumbing gap
and not a schema gap — both were verified present:

- **The baselines are live.** `prev_day_close` and `session_open` are
  refreshed from the exchange fields on every fold in `fold_into_bucket`,
  last-non-zero-wins. In the minute sampled, all **189,396** ticks carried
  both.
- **The write path is intact.** `ShadowSealRow::from_buffered_seal` copies
  `state.open_pct` / `state.open_gap_pct` / `state.close_pct_from_prev_day`
  verbatim into the ILP row, and the columns exist in the DDL.
- **The arithmetic was never written.** `open_bucket` initialises all three to
  `0.0`, and a workspace scan finds **no other production assignment to any of
  them**. The doc comments on `LiveCandleState` say "Stamped at seal time";
  nothing stamps.

This is the false-OK class: a column that exists, is documented, flows through
the writer, and is always zero reads to a consumer as "this instrument has not
moved", not as "this was never computed".

### Design

One pure method on `LiveCandleState`, called at every point a bar leaves the
aggregator:

```
stamp_seal_percentages():
    close_pct_from_prev_day = pct(close,        prev_day_close)
    open_pct                = pct(close,        session_open)
    open_gap_pct            = pct(session_open, prev_day_close)
```

where `pct(value, baseline)` returns `0.0` unless `baseline` is finite and
strictly positive **and** `value` is finite, and returns `0.0` again if the
quotient is not finite. Zero is the existing "not computable" sentinel for
these columns and stays so — a bar whose baseline never arrived is
indistinguishable from one that has not moved, and inventing any other value
would be worse.

`volume_pct_from_prev_day` and `oi_pct_from_prev_day` are deliberately NOT
stamped: their columns were dropped from the candle DDL (operator decision
2026-05-28 — spot instruments have no OI and indices have no volume), so
computing them would produce fields nothing reads.

**`change_pct` is not a fourth number.** `ShadowSealRow` already sets it equal
to `close_pct_from_prev_day`; that stays, and this item does not add a
separate meaning for it.

### The emission sites — all four, and why a helper is not enough

A bar leaves `AggregatorCell` at four points, and the fourth is the one a
careless fix would miss:

| # | Site | Why it must stamp |
|---|---|---|
| 1 | `consume_tick`, intraday bucket crossing (`ConsumeOutcome::Sealed`) | the ordinary path — most bars |
| 2 | `force_seal` | the IST-midnight / shutdown drain |
| 3 | `catch_up_seal` | the watermark sweep, which seals illiquid instruments on time |
| 4 | `consume_tick`, late refold (`ConsumeOutcome::AmendedLate`) | **the bar's `close` CHANGED.** A percentage stamped before the amend is stale for the row that is actually persisted |

Site 4 is the reason this is not a one-line change. A late tick re-writes
`close` on an already-emitted bar and the amended state is re-persisted; a
percentage computed at the original seal would then disagree with the `close`
sitting beside it in the same row.

### Edge Cases

| Case | Behaviour |
|---|---|
| Pre-open, no exchange baseline yet | `session_open` / `prev_day_close` are `0.0`; both percentages stamp `0.0`. Unchanged from today. |
| Baseline arrives mid-session (indices — the first several ticks carry no open) | last-non-zero-wins already handles it; every bar sealed after arrival stamps correctly. Bars sealed before it keep `0.0`, honestly. |
| `close` is a subnormal or non-finite value | the ingest gates upstream already refuse those; the `is_finite` test here is belt-and-braces and returns `0.0`. |
| Baseline is negative or non-finite | refused — `0.0`. A negative baseline would flip the sign of every percentage silently. |
| Quotient overflows to infinity | refused — `0.0`. Reachable only from a subnormal baseline that passed the positivity test. |
| A late refold changes `close` | site 4 re-stamps from the amended `close`. |
| Day boundary | `force_seal` clears the day-scoped state as it already does; the next day's first bar stamps `0.0` until its baseline arrives. |

### Failure Modes

| If this breaks | Blast radius |
|---|---|
| The stamp is wrong | a wrong percentage in a column that is zero today — strictly no worse than the status quo, and visible immediately in any query |
| A fifth emission site is added later without stamping | that path silently ships zeros again. **This is the real risk**, and it is what the guard test below exists for. |
| The stamp panics | it cannot: no indexing, no unwrap, no division without a positivity test |

### Test Plan

Unit tests on `LiveCandleState::stamp_seal_percentages` (pure, no I/O):

- a normal bar stamps all three correctly against hand-computed values
- the live NIFTY numbers from today used as a fixture
  (open 24,341.95 · prev close 24,334.55 · close 24,273.15 →
  intraday −0.28%, gap +0.03%)
- zero / negative / non-finite / subnormal baseline each stamps `0.0`
- non-finite `close` stamps `0.0`
- an unchanged price stamps exactly `0.0`, not a rounding artefact

Aggregator tests, one per emission site:

- an intraday crossing seal carries non-zero percentages
- `force_seal` carries them
- `catch_up_seal` carries them
- a late refold that moves `close` re-stamps to match the amended `close`

Guard test (the durable half):

- a source scan asserting every `ConsumeOutcome::Sealed` / `AmendedLate`
  construction and every `mem::replace` seal in `aggregator_cell.rs` is
  preceded by a stamp — so a fifth path added next month fails the build
  instead of shipping zeros

### Rollback

Revert the commit. The columns return to `0.0`, which is the state they have
been in since they were created — no migration, no schema change, no data to
undo. Nothing downstream reads them today, so nothing breaks in either
direction.

### Observability

No new metric, no new alarm, no cost. The evidence is the columns themselves:
the same live query in the table above returns non-zero on the first session
after deploy. That query is the acceptance test, and it is cheap enough to
re-run any day.

### Guarantee matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md` — the 15-row and
7-row matrices apply to this item as written there. Rows specific to it:
zero-alloc (three float divisions on existing struct fields, no allocation, no
new hot-path branch on the tick path — the stamp runs once per SEAL, not once
per tick); O(1) (constant work per bar); uniqueness/dedup unchanged (no key
column touched); recovery unchanged (the seal ring, spill and DLQ carry the
same struct they always did).


### Item 25 — CORRECTION (2026-08-26, same day, operator-caught)

I labelled the two columns the wrong way round in the plan above, the
artifact, and the chat summary. He caught it verbatim:

> "what eprcenatge change shdou l chekc with rpevd ay close right dude ami i
> rght dude? but for only for pre open 9.15 am open prcoe comapred with evry
> minute or seocdn closed rpcoe ddue am i irght dude what the fukis this dude
> okay? i believe you are wong wiht clauclting theis percnetga change and rpe
> open eprncetage dud eokay?"

| Column | Question | What I called it | **What it actually is** |
|---|---|---|---|
| `close_pct_from_prev_day` | close vs yesterday's close | "the day change" | **percentage change** |
| `open_pct` | close vs today's 09:15 open | "percentage change" | **pre-open percentage change** |
| `open_gap_pct` | 09:15 open vs yesterday's close | "pre-open percentage" | the overnight **gap** |

**He is right and the reasoning is his own, stated three times today:** the
09:15 open IS the pre-open call-auction equilibrium price
(*"the finalised pre open 9.12 close price as 9.15 am open price"*). So
"pre-open percentage change" means *how far this bar has moved from the
pre-open-determined open* — `open_pct`. And "percentage change" unqualified
means change on the previous close, which is the market convention every
screen in India uses.

**No arithmetic changes.** All three columns were already being stamped by
this item, and all three formulas were already correct; only my NAMES for
them were wrong. What changed is the documentation, the test labels, and —
importantly — the default the movers ranking must use: **rank by
`close_pct_from_prev_day`**, the percentage change, not by `open_pct`.

That last consequence is why this correction was worth chasing rather than
quietly amending: a gainers list ranked on the wrong column produces a
plausible, completely different list, and nothing about it would look wrong.

---

# WAVE 9 — the 2026-09-01 audit: the fifteenth socket, the folded refusal, and the unseeded pager

**Status of this wave:** IN_PROGRESS (the plan file's own Status stays IN_PROGRESS).
**Date:** 2026-09-01
**Authorised by:** Parthiban, 2026-09-01, verbatim: *"Fix everything I never ever wanted to
face any websocket disconnect or reconnect and there shouldn't be any tick loss no db
pressure no memory ram pressure nowhere our entire 16 websocket connections shoudl never
ever be dropped or lost at any point of time especially starting. 9 am till 3.49 pm per
current day should never ever be stopped or lost dude okay?"*

## Why this wave exists

A live read of the Monday 2026-08-31 session (CloudWatch, `Tickvault/Prod`, 115 metrics)
plus a four-agent source sweep. Session facts, all MEASURED:

| Fact | Value |
|---|---|
| Ticks ingested | 83,932,875 (week high) |
| **Permanently lost** | **0** — `tv_ticks_lost_total`=0, `wal_dropped`=0, dropped==spilled exactly |
| Refused by DB, rescued to disk | 11,362,297 (13.5%, 37x Friday) |
| QuestDB apply lag | 18 -> **80,217**, monotonic, never recovered |
| Alive connections | **14**, held min=max=14 for 8 hours |
| Reconnects / dial failures | 47 / 46, **all inside one 5-minute window at 08:50 IST** |

Four things the audit CORRECTED rather than found (recorded because this repo's history
says stale findings manufacture false work):

- "Six dead dashboard widgets" — **FALSE**. Those greps are removal comments
  (`dashboard.tf:97,105,112`); the widgets were retired 2026-07-15/17.
- "Dead alarm `order_fill_lag_high`" — **FALSE**. Deliberately dormant,
  `actions_enabled=false` (`order-side-alarms.tf:282`), guard-pinned.
- "87 invisible loss counters" — **OVERSTATED**. `loss_counter_visibility_guard.rs:477`
  already enforces shipped-or-logged. They lack a *chartable series*, not visibility.
- CLAUDE.md's "spot store projects ~34.9 GB" — **STALE**. `config/base.toml:930` sets
  `spot_days = 1`, not 35, so the projection is 721 MB, and the store commits **zero**
  bytes today (`[rest_candle_fold] enabled = false`, no writer).

## Design

**Item 9.1 — the fifteenth socket (the operator's stated #1).**
`tv_dhan_ws_alive_connections` peaked at 14. Two independent causes:
(a) the gauge's ceiling is **15, not 16** — the order-update socket is spawned by
`dhan_rest_stack.rs` and `AliveConnectionGuard::acquire` refuses non-market-data endpoints
(`dhan_feed_stack.rs:7993`), so 16 was never countable here;
(b) the 5th depth-200 socket (the day's biggest mover) is genuinely lost every session.
Depth attaches PRE-OPEN (~08:50, driven toward the 09:12 readiness deadline). At that
moment `fetch_movers` finds `candles_1m` empty (the aggregator folds from 09:15) AND its
pre-open tick fallback empty (capture starts 09:00), so `top_mover_pick` returns `None`
(`dhan_feed_stack.rs:7268`). Its `None` arm states *"The retry loop asks again"* — but
`depth_done = true` latches unconditionally at `:7615`, `outstanding_halves` (`:6823`)
never returns the depth half again, and the selection block short-circuits to
`DepthSelection::default()` (`:7183`). **A comment asserting a retry the code prevents.**
Fix: a separate `depth200_fifth_done` latch so the depth half stays outstanding for the
fifth socket ONLY, re-attempting the pick on the existing 60 s loop once a ranking exists
(from ~09:00 via ticks, ~09:16 via candles), and dialing just that one instrument on a
later loop iteration. Depth-20 and the four ATM depth-200 sockets are never re-planned.

**Item 9.2 — the folded refusal counter.** `dhan_feed_stack.rs:2052` folds FIVE distinct
reasons into `reason="out_of_session"`, and the 30 s read-out at `:4289-4311` deliberately
skips that field entirely. Suppression is CORRECT for true `out_of_session` (ticks outside
[09:00,15:40) are normal daily) and WRONG for the other four — `stale_trading_day` has a
recorded 8,898 fabricated bars (`:1969-1974`) and `out_of_band_timestamp` measured
2,008,916/session (`multi_tf_aggregator.rs:717`). Fix: split the single counter field into
five, widen `refusals()`, and add the two real-signal reasons to the EXISTING
AGGREGATOR-DROP-01 line and its emit condition. **No new metric name, no new EMF entry,
$0/mo** — which matters because the maximal month is ~$123.88 against a $125 cap and a
$117 auto-stop line, leaving ~3 names of headroom.

**Item 9.3 — the unseeded pager.** `tv_telegram_dropped_total` is created lazily in error
arms only, so an unregistered counter publishes no series and the FIRST episode is
invisible — making the documented alarm threshold of 3 behave as 4. Fix: seed all three
reason labels at `main.rs:1245` beside the existing seeds. Log-filter metric, **$0**.

**Item 9.4 — the boot-only gauge that reads as live.** `tv_dhan_feed_stack_connections`
is written once at bring-up (`:8651`) and never moves; it reads 1.0 while 14 sockets are
alive. Its own doc admits it, but it is EMF-shipped, so an operator reads a live count.
Fix: publish it on every dial and every park.

## NOT taken in this wave, deliberately

- **`ticks.ts` -> receipt clock.** This is the measured root cause of the DB pressure:
  `ticks` is `PARTITION BY HOUR` on exchange last-TRADE time and **10.0% of one day's
  64.3M ticks carried a ts more than an hour behind arrival**, so one commit in ten
  reopens a closed hourly partition and rewrites it (`tick_persistence.rs:1954`, measured
  live). Changing it is the single biggest lever AND changes `ts` semantics for every
  consumer — the cross-verify window and `partition_archive` both key on it. It needs
  `scripts/diagnose-write-amplification.sh` run on the box first. Specified, not shipped.
- **`QDB_CAIRO_O3_MAX_LAG` 60000000 -> 300000.** An agent proposed this as its #2 lever,
  claiming the unit is milliseconds (16.7 h). **REJECTED.** The comment block above it
  states every value there was *"VERIFIED against the running container on 2026-08-18 via
  SHOW PARAMETERS"*; QuestDB's `cairo.o3.max.lag` is microseconds (default 600000000 =
  10 min), so 60000000 = 60 s exactly as documented. Applying it would have set a
  0.3-second merge window. Recorded so nobody re-proposes it.
- **Gating the live lane's first dial on 09:00.** It would kill most of the 46 dial
  failures (they are pre-open TCP-RSTs — `market_hours_gate.rs:5-10` documents Dhan
  RSTing idle pre-open connections) but would cost the 09:00-09:15 pre-open candles the
  2026-08-28 operator directive requires. A 25 ms/slot stagger on the FIRST dial is the
  safe half and is deferred to its own wave with its own measurement.

## Edge Cases

- Fifth socket never fills because the whole session is genuinely flat: the socket goes
  unused and that is HONEST — the retry must stop at 15:30, not spin.
- `pool.admit` must refuse a sixth depth-200 socket even if the retry misfires.
- The retry must NOT re-plan depth-20 or the four ATM sockets (they are already subscribed;
  re-dialing would drop live books).
- Seeding a counter with `.increment(0)` must happen AFTER the metrics recorder is
  installed, or it resolves to a no-op recorder (same rationale as the existing seeds).
- Splitting the refusal field must not change the emitted metric label — the EMF processor
  folds label values, so the single `out_of_session` series must stay byte-identical.

## Failure Modes

- Retry loop dials a sixth socket -> Dhan error 805 -> `ParkReason::PoolOverflow`, permanent.
  Guarded by the `DEPTH_200_MAX_SOCKETS` check plus `pool.admit`.
- A late fifth-socket dial races the 15:30 attach hard stop (`:7094`) -> bounded by the
  same stop.
- Un-suppressing two reasons makes the 30 s line fire on a day where they are legitimately
  non-zero -> it is a log line, not a page; no alarm is added.

## Test Plan

- `outstanding_halves` gains a fifth-socket case: depth_done=true + fifth_done=false must
  still return the depth half. Pure function, unit-testable.
- A test that the fifth-socket retry stops at 15:30 and after first success.
- A test that the five split refusal fields sum to the previous single field.
- A test that the AGGREGATOR-DROP-01 emit condition fires on stale_trading_day > 0 and does
  NOT fire on true out_of_session alone.
- A test that all three telegram reason labels are seeded at boot.
- Guard re-bless: any source-scan guard pinning the touched lines.

## Rollback

Every item is additive and independently revertable. 9.1 is one boolean; reverting restores
today's 14-socket behaviour. 9.2/9.3 are counters and log fields with no metric-name change,
so no terraform, no EMF count move, no cost change. 9.4 is one gauge publish.

## Observability

No new EMF metric name and no new alarm in this wave — deliberate, given ~3 names of budget
headroom. 9.2 adds fields to an existing coded line (`AGGREGATOR-DROP-01`), which a
CloudWatch metric filter can later extract as a numeric value at $0.10/mo if a page is
wanted (the §2.3e precedent). 9.1 makes the fifth socket visible through the already-shipped
`tv_dhan_ws_alive_connections`. 9.4 repairs an already-shipped gauge.

### ⚠ Item 9.1 CORRECTED 2026-09-01, before any code was written

The design above says the fix is *"re-attempting the pick on the existing 60 s loop"*.
**That loop does not exist to retry on.** `dhan_feed_stack.rs:7758` RETURNS from the
attach task once `contracts_done && depth_done` and the top-up window is closed — and it
returns while handing `depth_commands` BY MOVE to the per-minute rebalance machinery
(`:7770-7775`), because holding them any longer closes those channels. So there is no
surviving loop iteration in which a late fifth-socket dial could run.

That makes item 9.1 an ARCHITECTURAL change, not a latch flip. The three available shapes,
none of them small:

| Shape | Cost |
|---|---|
| Keep the attach task alive past both halves to chase the fifth | Changes the single success-return, and delays the `depth_commands` hand-off that makes the ATM machinery live |
| Delay the depth dial until a ranking exists (~09:00 ticks / ~09:16 candles) | Forfeits pre-open depth, which the 2026-08-28 directive requires |
| Give the fifth socket to the rebalance machinery | That machinery tops up EXISTING sockets; it does not hold `pool` and cannot dial |

**One genuine de-risking finding, recorded for whoever takes this:** the fear in the
comment at `:7212` — that dialing the fifth later is unsafe — is smaller than it reads.
`PoolSupervisor::admit` (`pool_supervisor.rs:2367`) delegates to `budget.try_open`, which
is STATEFUL per endpoint, so a later dial cannot over-open: it either returns the next
slot (index 4, exactly `DEPTH_200_TOP_MOVER_SOCKET`) or refuses with `PoolBudgetRefusal`,
which the existing `Err` arm already handles. Over-dialing into Dhan error 805 is
therefore structurally impossible. And `build_feed_stack_plan(&mut pool, now, &[], &[],
&[fifth])` plans ONLY that socket, because `plan_pool` returns early on an empty set
(`:717`) — depth-20 and the four ATM sockets cannot be re-planned by accident.

**NOT IMPLEMENTED in this wave, deliberately.** It touches the live connection path in an
environment where neither Dhan nor QuestDB is reachable, so it cannot be validated here,
and a wrong move parks a socket permanently — the exact failure the operator is asking to
prevent. It is specified above and should land as its own PR with box validation.

**Also corrected:** the 16-vs-15 half is NOT a defect. `AliveConnectionGuard::acquire`
refuses non-market-data endpoints (`:7993`), and the order-update socket is spawned by
`dhan_rest_stack.rs`, so `tv_dhan_ws_alive_connections` has a structural ceiling of 15 and
"16" was never countable on this gauge. A full house is **15 on this gauge plus the
order-update socket**, not 16. The honest fix is to document the ceiling or count the
sixteenth — not to hunt a socket that was never in this gauge's scope.
