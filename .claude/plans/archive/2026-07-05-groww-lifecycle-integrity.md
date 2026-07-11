# Implementation Plan: Groww lifecycle integrity — flip disappeared rows, preserve first_seen, scope the last_seen bump

**Status:** VERIFIED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator) — audit-confirmed gap fixes assigned via coordinator, 2026-07-05

> **Guarantee matrices:** this plan carries the 15-row + 7-row guarantee matrix by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (the
> canonical matrix applies verbatim; per-item deltas are named in the items below).
> Honest 100% wording per `operator-charter-forever.md` §F applies.

## Design

Three audit-confirmed Groww lifecycle-integrity gaps, fixed in one PR on the
existing `persist_groww_instruments` cold-path chain (crates: `tickvault-core`,
`tickvault-storage`):

1. **[HIGH] Gap 1 — disappearance never flips the DATA row + duplicate daily audit
   rows.** A Groww instrument that disappears from the universe is absent from the
   daily full-row UPSERT batch, so its `instrument_lifecycle` master row stayed
   `active` forever — and because the DATA row never flipped, the audit diff
   re-emitted a NEW duplicate `expired` transition row EVERY day. Fix: after the
   audit rows are durably appended (§24 audit-first) and the DATA UPSERT runs,
   `persist_groww_instruments` applies a FEED-SCOPED in-place state flip for every
   non-active audit transition (`groww_disappearance_flips` filter →
   `instrument_lifecycle_persistence::update_lifecycle_state_for_feed`, WHERE
   `security_id = .. AND exchange_segment = '..' AND feed='groww' AND
   dry_run=false`). Mirror of the Dhan reconciler's disappearance UPDATE; NEVER a
   DELETE (SEBI §25); DEDUP keys unchanged (the UPDATE never touches a key column).
2. **[HIGH] Gap 2 — `first_seen_date` reset every day.** The daily UPSERT replaced
   the whole row with `first_seen_date = today` (SEBI/audit corruption). Fix: the
   prior `feed='groww'` snapshot is loaded ONCE in persist (hoisted out of the audit
   emit) — the SELECT now also reads `cast(first_seen_date as long)` — and
   `resolve_groww_first_seen_nanos` carries the prior value through the UPSERT
   (today only for a brand-new instrument or a NULL/0 prior), mirroring the Dhan
   `resolve_first_seen_nanos` guard in `crates/app/src/lifecycle_apply.rs`.
3. **[MED] Gap 3 — Dhan warm-skip bump too broad.**
   `build_bump_active_last_seen_sql` lacked feed/dry_run predicates, so the Dhan
   warm-skip `last_seen` bump mutated `feed='groww'` rows AND §27 dry-run rows.
   Fix: `WHERE lifecycle_state='active' AND feed='dhan' AND dry_run=false`.

All fixes keep: DEDUP keys unchanged; never-DELETE (SEBI); audit-first ordering
(§24 — the audit emit now returns `Option<rows>` and a failed append SKIPS the DATA
UPSERT + flips so a state change never lands without its forensic row); best-effort
degrade (GROWW-MASTER-01, with the NEW `stage="lifecycle_flip"` label documented in
`groww-shared-master-error-codes.md`); I-P1-11 composite keys.

## Plan Items

- [x] Gap 3: scope the warm bump + add the feed-scoped flip builder/exec in storage
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs
  - Tests: test_build_bump_active_last_seen_sql_shape_micros_and_active_filter,
    test_build_lifecycle_state_update_sql_for_feed_shape_and_predicates,
    test_build_lifecycle_state_update_sql_for_feed_never_touches_dedup_key_columns,
    test_build_lifecycle_state_update_sql_for_feed_sanitizes_injection,
    test_update_lifecycle_state_for_feed_returns_err_when_questdb_unreachable
- [x] Gap 2: prior snapshot carries first_seen; UPSERT preserves it
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_resolve_groww_first_seen_preserves_prior_and_falls_back,
    test_build_groww_lifecycle_rows_preserves_first_seen_on_day2,
    test_parse_groww_lifecycle_dataset_well_formed_and_skips,
    test_build_groww_lifecycle_select_sql_scopes_to_groww_feed
- [x] Gap 1: disappearance flips + §24 gating + single prior load in persist
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_disappearance_yields_one_expired_transition_and_flip,
    test_day_after_flip_emits_zero_duplicate_transitions,
    test_groww_disappearance_flips_filters_on_to_state,
    test_persist_wires_feed_scoped_disappearance_flips_after_upsert,
    test_persist_ensures_lifecycle_table_before_audit_emit,
    test_groww_audit_emitter_is_degrade_safe_and_wired
- [x] Runbook: document the new `lifecycle_flip` stage + the 2026-07-05 fixes
  - Files: .claude/rules/project/groww-shared-master-error-codes.md
  - Tests: N/A — docs (cross-ref test already satisfied: GrowwMaster01PersistFailed mentioned)

## Edge Cases

- Day-1 / fresh DB: empty prior → all `appeared`, zero flips (existing behaviour kept).
- Same-day re-boot after a flip: prior reads the flipped (non-active) state → diff
  no-op → ZERO duplicate audit rows and zero flips (idempotent).
- Reappearance day-3: prior non-active + present today → `reactivated`; the full-row
  UPSERT restores `active` + `expired_date=0`; first_seen still preserved from prior.
- Locked prior row disappearing: `classify_transition` skips (unchanged; test kept).
- Same `security_id` on a different segment: distinct identity (I-P1-11) — the
  first_seen resolver and the flip WHERE both key on `(security_id,
  exchange_segment)`; the flip additionally pins `feed='groww'` so a Dhan row
  sharing the composite key is never mutated.
- NULL prior `first_seen_date`: parses to the 0 sentinel → resolver falls back to today.
- Segment move: old key gets the non-active flip; the new key is UPSERTed `appeared`.
- Dry run (§27): no reads, no writes, no flips — counts logged against an empty prior.

## Failure Modes

- Prior-snapshot READ fails: GROWW-MASTER-01 `stage="lifecycle_audit"`; the audit +
  DATA UPSERT + flips are ALL skipped (an UPSERT without the prior would reset every
  first_seen); the prior-independent constituency append still runs; next boot
  re-runs idempotently.
- Audit APPEND fails: emit returns `None` → DATA UPSERT + flips skipped (§24); next
  boot re-emits.
- DATA UPSERT final failure: unchanged degrade (`stage="lifecycle"`); flips still run
  (they target yesterday's rows, untouched by the failed batch).
- Single flip UPDATE fails: GROWW-MASTER-01 `stage="lifecycle_flip"`, loop continues
  with the remaining flips; the audit row is durable and the next boot's diff
  re-emits + re-flips (converges).
- QuestDB not ready: unchanged `stage="readiness"` early return.

## Test Plan

- `cargo test -p tickvault-storage --features daily_universe_fetcher` (full suite) — green.
- `cargo test -p tickvault-core --features daily_universe_fetcher --no-fail-fast` —
  2554 passed, 2 failed: `test_fetch_api_bearer_token_errors_without_real_ssm` +
  `test_verify_public_ip_fails_without_real_ssm` — pre-existing ENVIRONMENTAL
  failures in this sandbox (live SSM reachable), unrelated to this diff.
- `cargo fmt --check` clean; `cargo clippy --no-deps --all-targets` adds zero
  warnings in the touched files.
- New unit + source-scan ratchet tests named per item above (13 new/extended tests).

## Rollback

Revert the single PR commit(s): both fixes are additive/pure-function changes on the
cold path (no schema change, no DEDUP-key change, no migration). Reverting restores
the prior (buggy) behaviour with no data cleanup required — rows flipped to
`expired_*` in the interim are semantically correct and are re-activated by the
normal UPSERT if the instrument reappears.

## Observability

- Counter: `tv_groww_master_persist_errors_total{stage}` gains the
  `lifecycle_flip` label (existing `lifecycle` / `constituency` / `lifecycle_audit`
  / `readiness` unchanged).
- Every failure arm logs `error!(code = ErrorCode::GrowwMaster01PersistFailed
  .code_str(), stage, ...)` (tag-guard compliant); success paths log `info!` with
  flip counts (`flips`, `flipped`).
- Runbook updated: `.claude/rules/project/groww-shared-master-error-codes.md`
  (2026-07-05 section) — triage via `mcp__tickvault-logs__tail_errors` +
  `questdb_sql` unchanged.
- Forensic ground truth: `instrument_lifecycle` rows now converge to `expired_*`
  on disappearance; `instrument_lifecycle_audit` stops accumulating daily
  duplicate `expired` rows.
