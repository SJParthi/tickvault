# Implementation Plan: Retention sweep detaches aged inactive partitions (was a silent no-op)

**Status:** APPROVED
**Date:** 2026-07-09
**Approved by:** Parthiban (operator) — small high-value bug-fix, this session
**Crate:** `storage` (`crates/storage/src/partition_manager.rs`)

Per-item 15-row + 7-row guarantee matrices: cross-reference
`.claude/rules/project/per-wave-guarantee-matrix.md` (applied below, item-scoped).

## Design

**Root cause (confirmed, `crates/storage/src/partition_manager.rs:253-258`).**
`detach_partitions_for_table` builds the partition-selection SQL as:

```
SELECT name FROM table_partitions('<table>')
WHERE minTimestamp < dateadd('d', -<N>, now())
AND active = true
```

QuestDB's `table_partitions()` exposes a boolean `active` column that is `true`
for exactly ONE partition per table — the currently-written one. That partition
is by definition the newest, so its `minTimestamp` is essentially never `< now -
90d`. The two predicates are therefore contradictory: the cutoff filter demands
an OLD partition, `active = true` demands the NEWEST partition. Their
intersection is empty on every run, so the daily retention sweep DETACHes ZERO
partitions — a silent no-op. QuestDB also refuses to detach the active partition
outright, so `active = true` was doubly wrong. The intent is the exact opposite:
detach the aged-out INACTIVE partitions, never the active one.

**Fix.**
1. Keep the cutoff/lookback filter EXACTLY where it is — server-side
   `minTimestamp < dateadd('d', -N, now())`. This preserves the existing
   retention-window semantics byte-for-byte and avoids re-deriving an IST cutoff
   in Rust (the `+5:30` timestamp landmine in `data-integrity.md`). Only the
   selection predicate is changed.
2. Project `active` alongside `name` and move the active-partition EXCLUSION
   into a pure, unit-testable Rust function `select_partitions_to_detach(rows)`
   that returns the names of rows where `active == false`. The active
   (current-day) partition is thus NEVER selected, in both the common case and
   the edge case where a table has not been written for > retention_days (its
   `active` partition would pass the SQL cutoff but is still excluded in Rust).
3. Replace `parse_partition_names` with `parse_partition_rows(json)` which maps
   the QuestDB JSON by column NAME (robust to column reordering) into
   `PartitionRow { name, active }`.

**Observability (non-vacuous proof).** Add two aggregate Prometheus counters
(static-label-free, cold-path — this is a once-daily post-market sweep):
`tv_partition_detach_selected_total` (incremented by the number of partitions
SELECTED for detach per table) and `tv_partition_detach_total` (incremented per
partition actually DETACHed). `selected_total > 0` is the live proof the sweep is
no longer a no-op.

**Scope guard.** LOCAL DETACH predicate + observability ONLY. No S3 archival, no
EBS snapshot, no new AWS paid service (operator-excluded). No indicator/strategy
change (§28). Existing per-partition detach-DDL failures keep their `warn!`
(unchanged — not part of this bug; `error_level_meta_guard` has no detach phrase,
and STORAGE-GAP-04 is S3-archive-specific so reusing it for a DETACH-DDL failure
would be a semantically-wrong runbook — avoided to prevent scope creep).

## Edge Cases

- **Only the active partition exists** (fresh table): SQL cutoff filters it out;
  even if it slipped through, `!active` excludes it → 0 detached. Correct.
- **Table not written for > retention_days**: its `active` partition passes the
  SQL cutoff but is excluded by `!active` → never detached (QuestDB forbids it).
- **Empty result set / table does not exist yet**: non-success HTTP → `Ok(0)`
  (unchanged); empty dataset → empty Vec → `Ok(0)`.
- **Malformed / missing `active` or `name` column, non-JSON body**: row is
  skipped (fail-closed — never selected for detach), no panic.
- **Cutoff boundary**: unchanged — still `minTimestamp < dateadd('d', -N, now())`
  server-side; this PR introduces no new boundary arithmetic.
- **Re-running the sweep (idempotency)**: a detached partition disappears from
  `table_partitions()` on the next run, so it is not re-selected; DETACH of an
  already-detached partition would return non-success and is counted as a warn,
  not a crash.

## Failure Modes

- **QuestDB unreachable / list query non-2xx**: `debug!` + `Ok(0)` (unchanged) —
  the caller in `main.rs` already treats a sweep error as non-critical `warn!`.
- **Individual DETACH DDL fails**: `warn!` + skip (unchanged); the aggregate
  `tv_partition_detach_selected_total` still records that work was attempted, so
  a selected-but-not-detached gap is visible via the two counters diverging.
- **Parse failure**: returns empty rows → 0 detached, logged upstream; never
  panics (no `unwrap`/`expect` in prod path).
- **F1 (bounded, honest envelope) — re-selecting an already-detached partition:**
  IF QuestDB's `table_partitions()` keeps listing a partition after DETACH
  (unverifiable here), the next daily sweep re-selects it → the re-issued DETACH
  DDL FAILS (already detached) → existing `warn!` arm, and `tv_partition_detach_total`
  (effect counter, successes only) stays honest — it is NOT inflated. Only
  `tv_partition_detach_selected_total` (intent) and per-partition `warn!` lines
  would accrue, bounded per run by the aged-partition count. **Recommendation
  (flagged follow-up):** once a live QuestDB confirms the `table_partitions`
  detached-row behaviour, add an `attached = false`/`detached = false` filter if
  needed. Not shipped now because an unverified column would risk a permanent
  no-op (worse than F1). Under the dominant QuestDB semantic (detached partitions
  drop out of the partition list) F1 does not occur and re-runs are idempotent.

## Test Plan

Pure-function unit tests (run under plain `cargo test -p tickvault-storage`, NO
live QuestDB):
- `test_select_partitions_to_detach_excludes_active_selects_inactive` — fixture
  `[active current, inactive aged1, inactive aged2]` → `[aged1, aged2]`, asserts
  the active name is NEVER present.
- `test_regression_old_active_true_predicate_would_detach_wrong_set` — proves the
  fix is NON-VACUOUS: the OLD `active == true` selection returns `[current]` (the
  forbidden active partition) while the NEW selection returns the aged inactive
  set and excludes `current`.
- `test_select_partitions_to_detach_empty_when_all_active` — all-active fixture →
  empty (never detaches the only/active partition).
- `test_select_partitions_to_detach_empty_input` — `[]` → `[]`.
- `parse_partition_rows` tests: valid 2-column dataset, empty dataset, invalid
  JSON, missing dataset, hour-format names, column-reordered dataset, row with
  missing `active` skipped (panic-safety).
- `test_detach_list_sql_uses_inactive_not_active` — the SQL builder contains
  `active = false` and NOT `active = true` (guards the regression at the source).

Existing tests kept green (`test_ticks_is_the_only_hour_table`, exempt-table
guards, coverage guard `partition_retention_coverage_guard.rs`).

Commands: `cargo test -p tickvault-storage`, `cargo fmt`,
`cargo clippy -p tickvault-storage -- -D warnings`,
`bash .claude/hooks/banned-pattern-scanner.sh`, `bash .claude/hooks/plan-verify.sh`.

## Adversarial 3-agent review outcome (2026-07-09)

- **hot-path-reviewer:** CLEAN — confirmed cold once-daily path; counters
  correct, no cardinality risk.
- **security-reviewer:** HIGH — partition `name` (from QuestDB JSON) was
  interpolated into `DETACH PARTITION LIST '<name>'` without validation
  (second-order injection under a hostile/MITM'd plain-http QuestDB response).
  FIXED inline: `is_valid_partition_name` fail-closed allowlist (digits/`-`/`T`,
  len ≤ 20) filters names in `select_partitions_to_detach`, so a malformed name
  is never selected. LOW (`table` param unvalidated) — documented: `table` is
  always a trusted compile-time/derived constant, never external input.
- **hostile bug-hunt:** F1/F2 MEDIUM — rely on the QuestDB semantic "does
  `table_partitions()` keep listing a partition after DETACH?", which is
  **UNVERIFIABLE in this offline environment** (no live QuestDB; Context7 quota
  exhausted). Honest decision: do NOT add an unverified `attached`/`detached`
  SQL column — a wrong column name would make the query 400 and turn the sweep
  into a PERMANENT no-op (strictly worse than the bounded F1). See Failure Modes
  for the bounded worst case. F3 LOW folded into the counter doc comment. F4 LOW
  resolved: the counter dashboard/alert meta-guards were retired with
  Grafana/Prometheus (no entry required; verified absent).

## Rollback

Single-file, self-contained change. Revert the PR commit — the retention sweep
returns to its prior (no-op) behaviour with zero data-migration or schema impact
(DETACH only ever moves aged partitions to a local `detached/` dir, never DROP,
so nothing is lost and rollback is instantaneous). The two new counters simply
stop incrementing. No config flag, no boot-order dependency.

## Observability

- `tv_partition_detach_selected_total` — partitions selected for detach across
  the sweep (the live proof the predicate now matches aged inactive partitions).
- `tv_partition_detach_total` — partitions actually detached.
- Existing `info!(total_detached, ...)` sweep-complete line and per-table
  `info!(table, detached, ...)` retained.
- Failures remain `warn!` (list query / detach DDL) — non-critical cold sweep,
  retried next post-market run.

## Guarantee matrices (item-scoped, per `per-wave-guarantee-matrix.md`)

15-row 100% matrix — proof artefacts for THIS item:
- Code coverage: new pure-function + parse tests; storage floor 91.2 held.
- Audit coverage: N/A — no new audit table (DETACH is not a per-event audit).
- Testing coverage: categories 1 (smoke), 2 (happy-path), 3 (error), 4 (edge),
  13 (panic-safety), 16 (serde/JSON), 22-scan covered by new tests.
- Code checks: banned-pattern + plan-verify + fmt + clippy `-D warnings`.
- Code performance: cold path (once-daily post-market) — no hot-path change; no
  DHAT/Criterion needed (N/A — not hot path).
- Monitoring: two new counters above.
- Logging: sweep info! + per-table info! retained; failures warn! (unchanged).
- Alerting: N/A — no new failure-mode alert (cold sweep; existing warn! path).
- Security: no external input beyond QuestDB JSON we already parse; parse is
  fail-closed on malformed rows.
- Security hardening: N/A.
- Bug fixing: this IS the bug fix; adversarial 3-agent review before PR.
- Scenarios: edge cases enumerated above with tests.
- Functionalities: `select_partitions_to_detach` + `parse_partition_rows` each
  have tests AND a call site in `detach_partitions_for_table`.
- Code review: adversarial 3-agent (hot-path + security + hostile) on the diff.
- Extreme check: `test_detach_list_sql_uses_inactive_not_active` +
  `test_regression_old_active_true_predicate_would_detach_wrong_set` fail the
  build if the predicate regresses.

7-row resilience matrix:
- Zero ticks lost: N/A — DETACH never touches live tick ingest; no new drop path.
- WS never disconnects: N/A.
- Never slow/locked: cold once-daily sweep; no hot-path allocation added.
- QuestDB never fails: unchanged — sweep errors stay non-critical `warn!`, never
  halt; DETACH is metadata-only, never DROP (SEBI-safe).
- O(1) latency: N/A — cold path; selection is a single O(n) pass over the small
  per-table partition list.
- Uniqueness + dedup: N/A — no DEDUP key touched.
- Real-time proof: two counters make the fix observable live.

## Plan Items

- [x] Confirm root cause with file:line evidence
  - Files: crates/storage/src/partition_manager.rs
- [x] Flip selection predicate to aged-inactive via pure function
  - Files: crates/storage/src/partition_manager.rs
  - Tests: test_select_partitions_to_detach_excludes_active_selects_inactive,
    test_regression_old_active_true_predicate_would_detach_wrong_set,
    test_detach_list_sql_uses_inactive_not_active
- [x] Add `parse_partition_rows` (name + active, column-name mapped)
  - Files: crates/storage/src/partition_manager.rs
  - Tests: parse_partition_rows_* tests
- [x] Add `tv_partition_detach_selected_total` + `tv_partition_detach_total`
  - Files: crates/storage/src/partition_manager.rs
- [x] Adversarial 3-agent review; fix CRITICAL/HIGH inline
