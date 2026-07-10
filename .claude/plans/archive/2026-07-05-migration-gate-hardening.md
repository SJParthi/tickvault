# Implementation Plan: MigrationGate hardening — F13/F14/F15 (index_constituency ts-pin TRUNCATE ordering)

**Status:** APPROVED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator) — FIX 16 delta-audit directive; coordinator-confirmed routing 2026-07-05 ("F13/F14/F15 go as a separate main-branch PR — just the three findings + ratchets")

Guarantee matrices: cross-referenced per `per-wave-guarantee-matrix.md`
(the canonical 15-row + 7-row matrices apply to every item below; this
plan adds no new tick-drop path, no hot-path allocation, and no DEDUP-key
change — the migration/gate chain is cold-path forensic-master code only).

## The three skeptic-confirmed findings (delta audit 2026-07-05)

| # | Sev | One line |
|---|---|---|
| F13 | High | The Groww writer's 120s `MigrationGate` wait is shorter than the Dhan lane's realistic path to the TRUNCATE on a Monday cold boot (infinite-retry CSV + TOTP cooldown + QuestDB wait + ~219K-row reconcile all precede the migration spawn) — rows appended on timeout are then wiped, and the timeout `warn!` ("Dhan lane likely OFF; no truncate can run") is FALSE on the dhan+groww profile, misdirecting triage. |
| F14 | High | Runtime Dhan enable / D2b lane cold-start retry runs the FIRST-EVER TRUNCATE hours after the Groww append — no bounded gate wait at append time can order against a migration whose first run happens later in the same process. |
| F15 | Medium | The gate is one-shot per process while the TRUNCATE is re-runnable per lane restart: a best-effort marker-write failure (or the documented delete-marker operator procedure) lets a later in-process TRUNCATE wipe rows behind a permanently-green gate. |

## Design

The root cause of all three findings is the SAME structural fact: the
one-shot, marker-gated `TRUNCATE TABLE index_constituency` migration is
reachable ONLY through the Dhan lane (`persist_index_constituency_mapping`,
spawned at the tail of `cold_build_daily_universe`), while the gate that
orders the Groww append against it assumes "timeout ⇒ no truncate this
process". Three coordinated changes remove the assumption instead of
patching around it:

1. **Process-global boot-prefix migration (closes F13 + F14).** A new
   `run_index_constituency_ts_pin_migration_at_boot(questdb)` task in
   `crates/app/src/index_constituency_boot.rs` is spawned from `main()`
   in the PROCESS-GLOBAL boot prefix — BEFORE the Groww activation
   watcher spawn (`run_groww_activation_watcher`) and regardless of
   `feeds.dhan_enabled` (the migration needs only the QuestDB config,
   never the Dhan lane). It runs a QUIET bounded QuestDB readiness
   probe (12 attempts × 5s via `shared_probe_client` `SELECT 1` — never
   BOOT-01/02 pages, mirroring `questdb_master_ready`), then
   `ensure_index_constituency_table` + the gate-aware migration wrapper.
   Worst case the gate is marked by ~T+75s (probe 60s + one bounded
   TRUNCATE HTTP attempt), comfortably inside the Groww writer's 120s
   gate wait — the migration no longer sits behind the Dhan lane's
   serial auth/CSV/reconcile chain, and a later runtime Dhan enable
   re-invokes a wrapper that is already in-process complete (change 2).
2. **In-process exactly-once wrapper (closes F15 + the concurrency
   race).** `MigrationGate` gains a `run_once: tokio::sync::OnceCell<()>`
   field. A new gate-parameterized
   `migrate_index_constituency_truncate_once_with_gate(questdb, gate)
   -> TsPinMigrationOutcome` executes the inner TRUNCATE body inside
   `run_once.get_or_init(..)` — so the inner body runs AT MOST ONCE per
   process, concurrent callers WAIT for the single run to finish (never
   two racing TRUNCATEs), and a marker-write failure (or an operator
   marker delete mid-session) can no longer re-TRUNCATE behind a green
   gate. The gate is marked complete after the once-cell resolves on
   every path. The existing public
   `migrate_index_constituency_truncate_once` delegates to the new fn
   with the process-wide gate, so both existing call sites (Dhan-lane
   `persist_index_constituency_mapping` step 0 + the new boot-prefix
   task) inherit the exactly-once semantics. A next PROCESS boot (fresh
   gate + fresh once-cell) still re-runs when the marker is absent —
   the cross-boot retry envelope is unchanged.
3. **Honest timeout wording (closes the F13 misdirection).** The Groww
   writer's gate-timeout `warn!` drops the false "Dhan lane likely OFF;
   no truncate can run" premise and states the truth: the boot-prefix
   migration has not completed within the bounded wait; the append
   proceeds best-effort and, if the one-shot truncate lands after it,
   the rows are wiped and re-persisted at the next boot/activation
   (the existing GROWW-MASTER-01 best-effort envelope). The
   `CONSTITUENCY_MIGRATION_GATE_TIMEOUT_SECS` const doc and
   `MigrationGate::wait`/gate docs are updated to the boot-prefix model.

Docs: dated 2026-07-05 note in
`.claude/rules/project/groww-shared-master-error-codes.md` (the FIX 13
section) recording the three findings + the hardened contract.

## Plan Items

- [x] Item 1 — In-process exactly-once migration wrapper + gate field (F15)
  - Files: crates/storage/src/index_constituency_persistence.rs
  - Tests: test_with_gate_skips_when_gate_already_complete,
    test_with_gate_runs_once_then_skips,
    test_with_gate_concurrent_callers_single_run,
    (existing MigrationGate tests unchanged)
  - Impl: `MigrationGate.run_once: tokio::sync::OnceCell<()>`,
    `TsPinMigrationOutcome { Ran, Skipped }`,
    `migrate_index_constituency_truncate_once_with_gate`, public wrapper
    delegates with `index_constituency_migration_gate()`; `wait`/gate doc
    updated (F14 false premise removed).

- [x] Item 2 — Process-global boot-prefix migration task (F13 + F14)
  - Files: crates/app/src/index_constituency_boot.rs, crates/app/src/main.rs
  - Tests: ratchet_ts_pin_migration_spawns_before_groww_watcher (source-scan
    on main.rs, wal_reinject ordering-ratchet pattern),
    test_ts_pin_readiness_constants_bound_inside_groww_gate_wait
  - Impl: `run_index_constituency_ts_pin_migration_at_boot` (quiet 12×5s
    `SELECT 1` probe via `shared_probe_client`, then ensure-table +
    gate-aware migration); spawned in main.rs before
    `run_groww_activation_watcher`.

- [x] Item 3 — Honest gate-timeout wording + wording pin (F13)
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: ratchet_gate_timeout_warn_is_honest (asserts the false
    "Dhan lane likely OFF" premise is ABSENT + the new wording present),
    ratchet_constituency_append_waits_for_migration_gate (existing, kept)

- [x] Item 4 — Rule-file note (dated 2026-07-05)
  - Files: .claude/rules/project/groww-shared-master-error-codes.md
  - Tests: N/A — docs (cross-ref tests still pass: no ErrorCode change)

## Edge Cases

- **QuestDB still starting at boot prefix:** the quiet probe absorbs up
  to 60s; if QuestDB is still down, the migration inner fails its own
  bounded HTTP call, the gate is marked (append proceeds), the marker is
  NOT written, and the NEXT process boot re-runs the migration in the
  boot prefix BEFORE that boot's Groww append — consistent ordering
  every boot.
- **Groww-only boot (Dhan OFF):** the boot-prefix task runs anyway (it
  needs only QuestDB config), so the gate is genuinely marked instead of
  relying on the old (false under D2b) "no truncate can run" premise.
- **Runtime Dhan enable hours later (D2b):** the lane's
  `persist_index_constituency_mapping` step-0 wrapper call finds the
  once-cell already resolved → `Skipped` → no late TRUNCATE ever fires
  in-process, even when the boot-prefix inner FAILED (deferred to next
  boot, which orders correctly again).
- **Concurrent wrapper invocations (boot-prefix still in flight when the
  Dhan lane reaches step 0):** `OnceCell::get_or_init` serializes — the
  second caller WAITS for the single inner run, then appends; no double
  TRUNCATE, no truncate-after-append inversion.
- **Operator deletes the marker mid-session (documented re-run
  procedure):** in-process the once-cell blocks a re-run (no silent wipe
  behind a green gate); the re-run happens at the next boot as the
  procedure intends.
- **Gate timeout still fires (pathological >120s QuestDB/probe delay):**
  the append proceeds best-effort with the HONEST warn; worst case the
  in-flight truncate lands after the append and the rows re-persist next
  boot/activation — the pre-existing GROWW-MASTER-01 envelope, now
  stated truthfully.

## Failure Modes

- Probe exhausts + TRUNCATE HTTP fails → `error!` (existing inner
  paths, Telegram-routable), marker not written, gate marked, next boot
  retries. No boot block, no feed impact.
- `shared_probe_client` build failure (HTTP-CLIENT-01 class) → probe
  loop degrades to warn + proceeds to the migration attempt (which
  builds its own client); bounded.
- Marker write failure after a successful TRUNCATE → in-process re-run
  now impossible (once-cell); next boot re-truncates + re-persists
  (DEDUP-idempotent) — the previously-documented "harmless" claim
  becomes actually true in-process.
- Boot-prefix task panic → tokio task dies; the Dhan-lane step-0 call
  (if the once-cell never initialized) still runs the migration before
  its own append; the Groww side times out once with the honest warn.
  Cold-path forensic table only — never ticks/orders.

## Test Plan

- `cargo test -p tickvault-storage` — new `with_gate` unit tests (skip /
  run-once / concurrent-single-run) + existing gate + dedup meta-guards.
- `cargo test -p tickvault-core --features daily_universe_fetcher` (lib)
  — wording-pin ratchet + existing gate-order ratchet + writer tests.
- `cargo test -p tickvault-app` (lib) — new main.rs source-order ratchet
  + existing `ratchet_migration_runs_before_csv_fetch`.
- `cargo fmt --check` + banned-pattern scan via the pre-commit hooks.
- Not live-verified in this PR (Assumed): the end-to-end Monday cold-boot
  ordering on the operator's Mac — the ratchets pin the source-order and
  exactly-once invariants that make the race unreachable by construction.

## Rollback

Single revert of the squash-merge commit restores the prior behavior
(bounded-gate-wait-only ordering). No schema change, no config change,
no data migration — the marker file format and path are untouched, so
rolling back cannot strand state. The boot-prefix task is additive; the
Dhan-lane call site remains in place throughout.

## Observability

- Existing signals unchanged and still fire: migration `info!`/`error!`
  lines (Telegram-routable), GROWW-MASTER-01 `error!` + 
  `tv_groww_master_persist_errors_total{stage}` on append failure.
- New: boot-prefix task logs one `info!` on spawn-side completion path
  (probe outcome + migration outcome), `warn!` when the probe exhausts.
- The gate-timeout `warn!` now carries honest triage guidance instead of
  the false Dhan-lane-OFF premise.
- Ratchets (build-failing): main.rs spawn-order source scan, wording pin,
  exactly-once unit tests.
