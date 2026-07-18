# Implementation Plan: Dead-Code Batch 1 — remove zero-caller storage persistence modules

**Status:** APPROVED
**Date:** 2026-07-18
**Approved by:** Parthiban (operator, standing dead-code removal order 2026-07-17, relayed via coordinator; LANE COMPLETE signal received)

## Plan Items

- [x] Delete `crates/storage/src/generic_candle_writer.rs` (whole module, 356 LoC) + its `crates/storage` lib.rs decl/comment
  - Files: crates/storage/src/generic_candle_writer.rs, crates/storage/src/lib.rs
  - Tests: cargo test -p tickvault-storage (scoped suite green)
- [x] Delete `crates/storage/src/prev_day_ohlcv_persistence.rs` (whole module, 554 LoC) + lib.rs decl; co-edit the two guard rows pinning `DEDUP_KEY_PREV_DAY_OHLCV`
  - Files: crates/storage/src/prev_day_ohlcv_persistence.rs, crates/storage/src/lib.rs, crates/storage/tests/o1_per_feed_doc_guard.rs, crates/storage/tests/dedup_segment_meta_guard.rs, crates/common/tests/dhan_api_coverage.rs
  - Tests: per_feed_market_data_dedup_keys_must_include_feed, doc_carries_the_honest_envelope_and_caveats_no_hallucination, every_storage_table_constant_has_a_retention_decision
- [x] SKIP `crates/storage/src/console_views.rs` — re-verification at current origin/main (80e2e832) found a LIVE production caller: `crates/app/src/candle_ddl_boot.rs:119` (`ensure_named_views`), pinned by `crates/app/tests/ensure_ddl_boot_wiring_guard.rs`. NOT deleted; no rule edit for `named_views_ensure` (the site is live again)
  - Files: (none — deliberate no-op, recorded for audit)
  - Tests: ensure_ddl_boot_wiring_guard (untouched, stays green)
- [x] Delete `crates/storage/src/instrument_fetch_audit_persistence.rs` (whole module, 985 LoC) + lib.rs decl; mark the `instrument_fetch_audit_ensure` site row RETIRED (dated 2026-07-18) in `.claude/rules/project/http-client-error-codes.md`. The `instrument_fetch_audit` TABLE stays (partition-manager string reference retained — not a code caller)
  - Files: crates/storage/src/instrument_fetch_audit_persistence.rs, crates/storage/src/lib.rs, .claude/rules/project/http-client-error-codes.md
  - Tests: every_error_code_variant_appears_in_a_rule_file (rule file keeps HttpClient01BuildFailed mention), cargo test -p tickvault-storage

## Per-Item Guarantee Matrix

See per-wave-guarantee-matrix.md — all 15 rows of the 100% Guarantee Matrix and
all 7 rows of the Resilience Demand Matrix apply to every item in this plan.
Deletion-only specifics: no new pub fns (rows "functionalities"/"code review"
satisfied by the guard co-edits + adversarial review), no hot-path change
(DHAT/Criterion rows N/A — no hot-path code touched), no new tick-drop path,
DEDUP/uniqueness preserved (tables + DB-side DEDUP config untouched), coverage
floors unaffected or improved (only never-executed code removed). Where "100%"
is claimed it is 100% inside the tested envelope, with ratcheted regression
coverage (the scoped storage suite + the dated guard-row comments).

## Design

Deletion-only batch 1 of the operator-ordered dead-code sweep over `crates/storage`
(inventory compiled 2026-07-18 against origin/main 0f5aa760; re-verified per-module
against current origin/main 80e2e832 before deletion). Each module had ZERO
non-test production references: their feeders died in the PR-C3 (2026-07-14)
Dhan-chain deletions or the cancelled SP4/SP5–SP7 Groww parity track
(groww-second-feed-scope §33.2). QuestDB TABLES (`prev_day_ohlcv`,
`instrument_fetch_audit`) are RETAINED — retention/archival is driven by the
partition_manager sweep-list STRINGS, which are not code callers and are not
touched. No new pub fns, no hot-path change, no behavior change on any live path.
Guard co-edits follow the merged Stage-2 dead-WS-sweep precedent exactly (dated
comment + row removal where a declaration site died). `console_views.rs` was in
the batch scope but is SKIPPED — it regained a live caller (`candle_ddl_boot.rs`)
after the inventory baseline.

## Edge Cases

- Inventory staleness: origin/main advanced 0f5aa760 → 80e2e832 between inventory
  and execution; every module re-grepped at current HEAD. Result: console_views
  found ALIVE (skipped); prev_day_ohlcv gained a second guard reference
  (`dedup_segment_meta_guard.rs:289`) — co-edited with a dated comment.
- Table-name STRING references (`partition_manager.rs`, `partition_archive.rs`)
  are retention-sweep data, not callers — deliberately untouched so the SEBI
  tables keep their retention/archival decisions.
- `partition_retention_coverage_guard.rs` derives requirements from src
  `const *TABLE*` decls (constants → sweep-lists direction): removing the
  constants while keeping the sweep strings is the safe direction (fewer
  requirements); verified green in the scoped test run.
- `dhan_api_coverage.rs` orphan scan counts CODE-line consumers only; the deleted
  module was a doc-comment mentioner — one stale filename dropped from its
  doc-comment example list.
- The o1 architecture doc (`docs/architecture/o1-per-feed-uniqueness-dedup-mapping.md`)
  keeps its `prev_day_ohlcv` TABLE row — still true (DB-side DEDUP config
  persists); only the doc↔writer-const binding rows in the guard are removed.

## Failure Modes

- A hidden caller (macro/string-dispatch/test) missed by grep → caught by
  `cargo test -p tickvault-storage` + workspace grep re-verification + the
  adversarial review pass; any compile failure blocks the PR.
- Guard-test weakening risk: removing the two `DEDUP_KEY_PREV_DAY_OHLCV` guard
  rows could mask a future per-feed-identity regression — mitigated by dated
  comments (the DEDUP_KEY_TICKS precedent) and by the table itself staying
  read-only with DB-side DEDUP intact; a future writer rebuild must re-add its
  constant + guard rows.
- Rule-file drift: the `instrument_fetch_audit_ensure` row is marked RETIRED
  (not silently removed) so `error_code_rule_file_crossref` keeps its
  HTTP-CLIENT-01 mentions and future readers see the dated decision.
- CI divergence from local scoped run → the full per-crate battery + All Green
  fan-in re-runs everything on the PR.

## Test Plan

- `cargo test -p tickvault-storage` (scoped per testing-scope.md — all changes in
  crates/storage except one comment in crates/common/tests + rule/plan files).
- `cargo test -p tickvault-common --test dhan_api_coverage` (the one edited
  common test file).
- `cargo clippy -p tickvault-storage --no-deps -- -D warnings` + `cargo fmt --check`.
- Workspace grep proving zero remaining references to
  `GenericCandle1mWriter | prev_day_ohlcv_persistence |
  instrument_fetch_audit_persistence` in `crates/**`.
- `bash .claude/hooks/banned-pattern-scanner.sh` (repo scanner).
- Adversarial review: security-reviewer + hostile general worker on the diff.

## Rollback

Deletion-only, single-branch: `git revert` of the batch commit restores all
three modules, the lib.rs decls, and the guard rows verbatim (no data
migration, no schema change, no config change — the QuestDB tables were never
touched). Until merge, closing the PR + deleting branch
`claude/deadcode-batch1-storage` is a complete rollback.

## Observability

No live metric, alarm, or Telegram surface changes: the deleted modules' only
coded emit (`HTTP-CLIENT-01`, site `instrument_fetch_audit_ensure`) had zero
reachable call sites, and `HttpClient01BuildFailed` retains many live emitters
(http_client.rs, boot_probe.rs, oms_wiring.rs, …) so no ErrorCode is orphaned
and the paging-filter drift guard sees no drift. The rule-file row is marked
RETIRED with a dated note (never silently dropped). Coverage floors: deletion
removes only never-executed module code + its in-module tests — per-crate
coverage is unaffected or improved.
