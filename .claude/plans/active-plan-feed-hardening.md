# Implementation Plan: Feed Hardening — bound the limits, alarm the silence, automate the recovery

**Status:** APPROVED
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
  - At T+90s after subscribe, assert `frames_received > 0` **per instrument class**;
    force-cycle the slot and page if a class is dead.
  - Maintain `expected: HashSet<(sid, segment)>`; at T+120s diff against first-seen.
    Auto-resubscribe the delta on a spare slot, max 3 rounds, then page **with the list**.
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
