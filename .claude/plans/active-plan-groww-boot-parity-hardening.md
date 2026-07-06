# Implementation Plan: Groww Boot Parity Hardening (FIX 13)

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — standing authorization tonight (2026-07-04 session): "fix everything". Deadline context: nothing blocks the operator tonight (all failure modes self-heal); land before Monday 08:30 IST.

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` —
> every item below inherits them; per-item specifics live in the Test Plan +
> Observability sections. Scope is COLD-path boot/persist hardening in
> tickvault-storage / tickvault-core / tickvault-app only — no hot tick path,
> no QuestDB schema change, no new ErrorCode, no WebSocket scope change.

## Design

Three cohesive Groww boot-parity gaps, all cold-path:

- [x] (a) **Truncate ordering gate.** The one-shot marker-gated
  `migrate_index_constituency_truncate_once` runs `TRUNCATE TABLE
  index_constituency`. QuestDB has NO row-level `DELETE ... WHERE` (only
  TRUNCATE / DROP PARTITION — verified against the codebase: every clear in
  the repo uses TRUNCATE), so a feed-scoped delete of only `feed='dhan'` rows
  is impossible. The truncate therefore wipes `feed='groww'` rows the
  fire-and-forget Groww persist may have already written — the two are
  unordered tokio spawns at boot. Fix: a `MigrationGate` (AtomicBool +
  tokio::sync::Notify; instance-testable struct + one process-wide static
  accessor) in `crates/storage/src/index_constituency_persistence.rs`. The
  migration marks the gate complete on EVERY exit path (outer wrapper calls
  the inner body then marks — a future early return cannot forget). The Groww
  shared-master writer awaits the gate (bounded 120s) before its
  `index_constituency` append. Additionally the ensure+migrate calls move to
  the TOP of `persist_index_constituency_mapping` (before the niftyindices
  CSV fetch) so the gate opens regardless of CSV failures while the
  truncate→write order is preserved.
  - Files: crates/storage/src/index_constituency_persistence.rs, crates/app/src/index_constituency_boot.rs, crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_migration_gate_wait_returns_after_mark, test_migration_gate_times_out_when_not_marked, test_migration_gate_is_complete_flag, test_migration_gate_static_accessor_is_stable, ratchet_migration_runs_before_csv_fetch, ratchet_constituency_append_waits_for_migration_gate

- [x] (b) **Readiness gate + bounded retry in `persist_groww_instruments`.**
  Each stage is one-shot today: a transient QuestDB hiccup at activation time
  loses the day's `feed='groww'` master rows until the next boot. Fix: a QUIET
  QuestDB readiness probe (`shared_probe_client`, `SELECT 1`, 3 attempts, 5s
  backoff — deliberately NOT `wait_for_questdb_ready`, which emits BOOT-01/02
  pages reserved for the boot path), then bounded retry (3 attempts, 2s/4s
  exponential backoff) around each append stage (lifecycle, constituency).
  Final failure keeps the EXACT existing degrade-safe semantics:
  `error!(code = GROWW-MASTER-01)` + `tv_groww_master_persist_errors_total{stage}`
  + return. New `stage="readiness"` label when the probe never succeeds.
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_master_persist_backoff_secs_ladder, test_master_persist_should_retry_boundaries, test_classify_persist_failure_readiness_stage

- [x] (c) **Boot Telegram Groww subscribe counts.** `activate_groww_lane`'s
  `Ok(set)` arm never calls `feed_health.set_subscribed(Feed::Groww, ...)`;
  the bridge only sets it when the sidecar later reports status, so the
  boot/readiness Telegram feed lines show unknown Groww counts. Fix: mirror
  the Dhan counterpart (`crates/app/src/main.rs` subscription-plan arm) —
  `feed_health.set_subscribed(Feed::Groww, resolved_stocks as u64, indices as u64)`
  in the `Ok(set)` arm before the persist spawn. Idempotent overwrite: the
  bridge's later sidecar-status set stays authoritative.
  - Files: crates/app/src/groww_activation.rs
  - Tests: ratchet_activation_sets_groww_subscribed_counts

- [x] (d) **Rule-file update.** Dated 2026-07-04 note in
  `.claude/rules/project/groww-shared-master-error-codes.md` covering (a)+(b)+(c)
  and the new `readiness` stage label.
  - Files: .claude/rules/project/groww-shared-master-error-codes.md
  - Tests: existing `error_code_rule_file_crossref` stays green (GROWW-MASTER-01 mention retained)

## Edge Cases

- Groww-only mode (Dhan lane OFF): `persist_index_constituency_mapping` never
  spawns → gate never marks → the Groww constituency write waits the bounded
  120s once per activation, then proceeds. CORRECT: the truncate cannot run
  in that mode, so there is no wipe risk — the wait is pure cost, bounded.
- Migration FAILS (QuestDB down): the gate still marks complete (marker NOT
  written → next boot retries the truncate). Groww proceeding is safe enough:
  worst case the NEXT boot's truncate wipes rows this boot wrote, and that
  same next boot re-persists them (idempotent DEDUP UPSERT). Once the
  truncate succeeds once, the marker makes it permanent.
- Notify race: the waiter registers `notified()` BEFORE re-checking the done
  flag (`notify_waiters` wakes only already-registered waiters).
- Fresh-DB Day-1: readiness probe passes once QuestDB answers `SELECT 1`;
  ensure-table calls create the tables; the audit emit sees an empty prior
  (existing behaviour unchanged).
- dry_run=true: the skip path is unchanged — no probe, no gate wait, no appends.
- Retry-storm bound: 3 attempts × 2 stages × ≤4s backoff plus ≤2×5s probe
  backoff ≤ ~25s extra cold-path latency worst case; activation itself is
  never blocked (the fire-and-forget spawn is unchanged).
- set_subscribed double-set: activation sets watch-set counts; the bridge's
  later sidecar-status set overwrites with live counts — the
  `FeedHealthRegistry::set_subscribed` API documents idempotent overwrite.

## Failure Modes

- QuestDB never ready → `stage="readiness"` GROWW-MASTER-01 `error!` +
  counter + return; next boot/activation re-runs. No panic, no unwrap,
  activation never blocks.
- Append fails all 3 attempts → the existing per-stage GROWW-MASTER-01
  `error!` + counter + return (semantics unchanged, just 2 retries first).
- Gate wait timeout → `warn!` + proceed (degrade-safe, honest envelope above).
- The migration marks the gate on ALL exits via the outer-wrapper pattern —
  a single mark site, unmissable.
- No new ErrorCode variants — GROWW-MASTER-01 covers every new failure arm
  (the stage label distinguishes), so the cross-ref + tag guards stay green.

## Test Plan

- Scoped per `.claude/rules/project/testing-scope.md` (no crates/common
  change → NOT workspace):
  - `cargo test -p tickvault-storage index_constituency`
  - `cargo test -p tickvault-core --features daily_universe_fetcher shared_master_writer`
  - `cargo test -p tickvault-app groww_activation index_constituency_boot`
- Unit: MigrationGate (mark→wait true; bounded timeout false on a fresh
  instance; is_complete flag; static accessor stability), backoff ladder
  values, should_retry boundaries, readiness stage classifier.
- Ratchets (source-scan, per the repo's wal_reinject / groww_bridge pattern):
  migration-before-CSV-fetch ordering in index_constituency_boot.rs;
  gate-await-before-constituency-append in shared_master_writer.rs;
  `set_subscribed(Feed::Groww` present in groww_activation.rs.
- `cargo fmt --check` + clippy on touched crates.

## Rollback

Single squash-merge commit; `git revert <sha>` restores the prior one-shot
behaviour exactly. No schema change, no config change, no new table, no
marker-file format change — the gate is purely additive in-process state, so
reverting cannot strand anything on disk or in QuestDB.

## Observability

- Existing counter `tv_groww_master_persist_errors_total{stage}` gains the
  `readiness` label value (no new counter name → no new alert-rule
  requirement; the existing GROWW-MASTER-01 triage rule covers it and the
  runbook is updated in item d).
- New `warn!` lines: gate-wait timeout; per-attempt retry warns carrying
  `attempt` + `backoff_secs` fields.
- GROWW-MASTER-01 `error!` lines unchanged in shape (code field intact for
  the tag guard).
- The boot/readiness Telegram feed lines and the /feeds page now show REAL
  Groww subscribed counts at activation time (set_subscribed) instead of
  unknown-until-sidecar-status.
