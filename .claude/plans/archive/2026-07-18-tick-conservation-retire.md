# Implementation Plan: Retire the tick-conservation daily audit (dead-WS sweep follow-up)

**Status:** APPROVED
**Date:** 2026-07-18
**Approved by:** Parthiban (operator, via coordinator dispatch 2026-07-18 — "retire (or re-scope, evidence decides) the tick-conservation daily audit"; evidence confirmed RETIRE)

## Design

The 15:40 IST daily tick-conservation audit reconciles WAL live-feed frame
counts vs tick-processor outcome counters vs `ticks` rows. Every input is
dead on the REST-only runtime: the Dhan live WS retired 2026-07-13 (PR-C2/C3),
the Groww live feed retired 2026-07-15 (#1581), the tick writer/processor
chain was deleted in the stage-2 dead-WS sweep 2026-07-17 (#1631) — no code
writes WAL LiveFeed frames, no processor counters exist, nothing writes the
`ticks` table. Every run since can only record `partial`/no-data rows and the
`tick-conserve-01` CloudWatch filter can only false-page on a meaningless
residual. Decision: RETIRE (the dead-WS sweep house pattern — #1631 / C4).

Scope (crates: **app**, **storage**, **common** + terraform/docs/rules):
1. Relocate the two LIVE-consumed helpers out of the doomed module FIRST:
   `ws_wal_dir()` → `crates/app/src/boot_helpers.rs` (boot/infra home; the
   STAGE-C replay + archive-prune + confirm sites in main.rs and the
   disk_retention_wiring_guard consume it); `parse_questdb_count()` →
   `crates/app/src/feed_scoreboard_boot.rs` (its sole surviving consumer).
   Unit tests move with them.
2. Delete `crates/app/src/tick_conservation_boot.rs` + the `lib.rs` mod decl
   + the main.rs spawn call + `spawn_daily_tick_conservation_task` fn.
3. Delete `crates/storage/src/tick_conservation_audit_persistence.rs` + its
   `lib.rs` mod decl. The QuestDB `tick_conservation_audit` TABLE is NEVER
   dropped (SEBI retention) — no DDL drop anywhere; the
   `partition_manager.rs` DAY-table row stays (the table persists).
4. Delete `crates/storage/src/ws_frame_spill.rs` conservation-only block
   (`count_frames_for_ist_day`, `WalDayFrameCounts`, `classify_frame_for_day`,
   `segment_creation_nanos`, `CONSERVATION_SEGMENT_LOOKBACK_DAYS`) — keep
   `ist_day_of_wall_nanos` only if the module's own surviving tests use it,
   else delete with the block. The archive-prune (`WS_WAL_ARCHIVE_RETENTION_SECS`)
   path is untouched.
5. Delete `ErrorCode::TickConserve01DailyResidual` (variant + code_str +
   severity + runbook_path + all() + catalogue count) and the
   `TICK-CONSERVE-` prefix from the source_scan allowlist with a dated note
   (the C4-sweep pattern).
6. Retire the `tick-conserve-01` `error_code_alerts` entry in
   `deploy/aws/terraform/error-code-alarms.tf` with a dated 2026-07-18 note
   (dead-filter rule: zero emit sites); update the entry count comment.
7. `observability-architecture.md`: remove TICK-CONSERVE-01 from the active
   paging list; add a dated "Retired paging entries" sentence.
8. `tick-conservation-audit-error-codes.md`: dated RETIREMENT banner atop;
   content retained as historical audit.
9. `.claude/triage/error-rules.yaml`: replace the
   `tick-conserve-01-daily-residual-escalate` rule with a dated retirement
   comment.
10. Delete `crates/app/tests/tick_conservation_failsoft.rs` +
    `tick_conservation_wiring_guard.rs`; edit
    `ensure_ddl_boot_wiring_guard.rs` (drop the tick_conservation entry) and
    `disk_retention_wiring_guard.rs` (pin the new `boot_helpers::ws_wal_dir`
    literal). Update `constants.rs` doc comment referencing
    `count_frames_for_ist_day` and feed_scoreboard_boot doc refs to the dead
    module.
- Files: crates/app/src/tick_conservation_boot.rs, crates/app/src/lib.rs,
  crates/app/src/main.rs, crates/app/src/boot_helpers.rs,
  crates/app/src/feed_scoreboard_boot.rs,
  crates/storage/src/tick_conservation_audit_persistence.rs,
  crates/storage/src/lib.rs, crates/storage/src/ws_frame_spill.rs,
  crates/common/src/error_code.rs, crates/common/src/constants.rs,
  deploy/aws/terraform/error-code-alarms.tf,
  .claude/rules/project/observability-architecture.md,
  .claude/rules/project/tick-conservation-audit-error-codes.md,
  .claude/triage/error-rules.yaml,
  crates/app/tests/{tick_conservation_failsoft.rs,tick_conservation_wiring_guard.rs,ensure_ddl_boot_wiring_guard.rs,disk_retention_wiring_guard.rs}
- Tests: existing `test_ws_wal_dir_default` + `test_parse_questdb_count`
  relocate with their fns; crossref tests
  (`every_error_code_variant_appears_in_a_rule_file`,
  `every_rule_file_code_has_an_enum_variant`) and
  `error_code_paging_filter_drift_guard` must stay green.

## Edge Cases

- `ws_wal_dir()` is the single-source-of-truth WAL dir for the SURVIVING
  STAGE-C boot replay + archive prune + confirm sites — relocation must be a
  pure move (same env var, same default) or boot replay silently reads a
  different dir. The disk_retention_wiring_guard pins the call-site literal
  and must be updated in lockstep.
- The reverse crossref check only scans TRACKED_PREFIXES; `TICK-CONSERVE-`
  is not tracked, so the rule file's historical mentions need NO reverse
  allowlist entry — but the forward check requires the variant be GONE
  before the runbook_path row is deleted (delete both in one commit).
- The drift guard fails on a tf entry with zero emit sites AND on a doc
  paging-list mention with no tf entry — tf + doc + emit sites must all be
  edited in the same commit.
- `ist_day_of_wall_nanos` in ws_frame_spill.rs is used by that module's own
  tests — verify before deleting; keep if any surviving test consumes it.
- The `tick_conservation_audit` table stays in
  `partition_manager.rs::DAY_PARTITIONED_TABLES` — the table persists and
  its aged partitions must keep archiving (SEBI).

## Failure Modes

- Deleting the module before relocating the helpers breaks main.rs (STAGE-C
  replay + archive prune) and feed_scoreboard_boot.rs → compile failure;
  mitigated by relocation-first ordering.
- Missing a dead ref (source_scan prefix, triage yaml, guard literal) →
  build-failing ratchet or drift-guard red; mitigated by wide grep for
  `TICK-CONSERVE\|TickConserve\|tick_conservation\|tick-conserve` after
  edits + scoped test run.
- Accidentally dropping the QuestDB table or its DDL → SEBI violation;
  mitigated: NO DDL drop is written anywhere, the persistence module is
  deleted (writer only), the table row in partition_manager stays.
- Gitignored test-count/untested-pubfn baselines drift from main → pre-push
  ratchet failure; pre-approved reset to true tree values.

## Test Plan

- `cargo test -p tickvault-app -p tickvault-storage -p tickvault-common`
  (scoped per testing-scope.md; common touched → workspace escalation is
  noted, but the deletion is leaf-ward: common's own crossref/drift guards
  are the risk surface and run in the scoped set; CI runs the full battery).
- `bash .claude/hooks/banned-pattern-scanner.sh` clean.
- `cargo fmt --check` clean.
- Relocated unit tests pass in their new homes.
- Post-edit grep proves zero live references to the deleted symbols.

## Guarantee Matrix

See per-wave-guarantee-matrix.md — all 15 rows of the 100% Guarantee Matrix
and all 7 rows of the Resilience Demand Matrix apply to every item in this
plan (a pure-deletion PR: scoped tests + banned-pattern scanner + crossref /
tag / drift / catalogue ratchets are the mechanical proof surface; no new
tick-drop path, no hot-path change, no DEDUP-key change, the SEBI table is
never dropped).

## Rollback

Single revert of the one squash-merged PR restores the module, writer,
variant, tf entry, and docs — the QuestDB table was never touched, so replay
is data-safe. Until merge, `git checkout origin/main -- <path>` restores any
file individually.

## Observability

No new metrics/alarms. Removals: the `tick-conserve-01` CloudWatch
log-filter alarm (dead filter — zero emit sites; ~-$0.10/mo, dated note in
error-code-alarms.tf; no EMF allowlist change — grep confirms no
`tv_tick_conservation_*` name in cloudwatch-agent.json/user-data.sh.tftpl,
the counters were /metrics-local). The rule file gains the dated retirement
banner; observability-architecture.md's paging list stays lockstep with tf
via the drift guard. The `tick_conservation_audit` table remains queryable
for historical forensics.
