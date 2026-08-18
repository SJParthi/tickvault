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

- [ ] **Item 5 — Disk: automatic purge + raw ticks off the database (#33, #55)**
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
