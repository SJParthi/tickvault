# Implementation Plan: PR-C — Dual-Feed Groww Master Writer Hardening Tests

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** Parthiban (operator brief 2026-06-28)

Closes the 6 test/hardening gaps found auditing the merged Groww shared-master
writer (#1230) + Groww auth (#1231). Touches `crates/core` (the writer + its
tests) and adds one source-scan guard in `crates/storage`; references
`crates/common` (the `ErrorCode`/`Feed` types). Test-only + tiny pure helper
extraction — NO behaviour change to the cold-path persist flow.

---

## Design

The Groww shared-master writer (`crates/core/src/feed/groww/shared_master_writer.rs`)
persists the daily `GrowwWatchSet` into the SAME shared `instrument_lifecycle` +
`index_constituency` QuestDB tables Dhan uses, every row tagged `feed='groww'`
(operator lock `groww-second-feed-scope-2026-06-19.md`; feed-in-key override
2026-06-28). The network orchestration `persist_groww_instruments` is correctly
`// TEST-EXEMPT` (I/O), but its DECISION LOGIC was untested. This PR extracts the
two pure decision points into pure fns so they ARE testable, then ratchets the 6
gaps with tests:

- **F1** — pure `should_skip_master_append(dry_run)` gate; dry-run builds rows
  but appends nothing (rule §27 isolation).
- **F2** — pure `classify_persist_failure(stage)` returning
  `(stage, ErrorCode::GrowwMaster01PersistFailed)`; the calling arm only
  logs+counts+returns (degrade-safe, never panics/aborts).
- **F3** — feature-off `persist_groww_instruments` is a compiled no-op.
- **F4** — source-scan ratchet: both storage append fns wrap every string/symbol
  row field in `sanitize_ilp_*`.
- **F5** — brownfield: a legacy `feed=""`/NULL row is distinct from dhan + groww
  under the composite key.
- **F6** — Groww IST-midnight nanos == Dhan reconciler date→nanos convention;
  pin the hardcoded NTM `index_name` assumption with a comment + test.

The two new pure helpers are `#[must_use] fn`s with no allocation and no I/O, so
the network path stays `// TEST-EXEMPT` while its logic is now covered.

## Edge Cases

- `dry_run=true` with a NON-empty set: rows MUST still be BUILT (builders return
  len>0) but NOT appended — proves "build-but-don't-write" isolation.
- `classify_persist_failure` called with an unexpected stage string: total fn,
  never panics; still returns the GROWW-MASTER-01 code.
- F3: feature OFF — the stub must compile and return without referencing any
  storage symbol (the master tables don't exist in that build).
- F5: three feed tuples `("", id, seg)`, `("dhan", id, seg)`, `("groww", id, seg)`
  — all 3 distinct, so a backfilled NULL-feed legacy row never collides.
- F6: epoch day (0), day+1, a real recent date, and an unparseable date (→0) —
  Groww formula must equal Dhan's `date_naive().and_hms_opt(0,0,0).and_utc()`
  nanos for every parseable date.

## Failure Modes

- A future edit removes the dry-run early-return → F1 test fails (skip-gate
  returns false would mean an append in dry-run).
- A future edit makes the persist-failure arm panic/abort instead of
  log+count+return → F2 ratchet (source-scan + classify totality) fails.
- A future edit writes a Groww symbol/string raw (un-sanitized) into either
  append fn → F4 source-scan guard fails the build (ILP injection ratchet).
- A future schema change drops `feed` from the composite identity → F5 distinctness
  fails (already pinned by `dedup_segment_meta_guard` for the key; F5 pins the
  in-memory tuple coexistence incl. the legacy NULL case).
- A future edit diverges the Groww date→nanos formula from the Dhan convention
  → F6 equality test fails.

## Test Plan

All new tests live in `crates/core/src/feed/groww/shared_master_writer.rs::tests`
except the F4 source-scan guard which lives in
`crates/storage/tests/groww_master_ilp_sanitize_guard.rs`.

| Item | Test | Asserts |
|---|---|---|
| F1 | `test_persist_dry_run_skips_append` | `should_skip_master_append(true)==true`, `(false)==false`; rows built len>0 in dry-run |
| F2 | `test_persist_failure_classifies_groww_master_01_without_abort` | `classify_persist_failure("lifecycle"/"constituency"/other)` total, returns GROWW-MASTER-01; source-scan: failure arm only logs+counts+returns |
| F3 | `test_persist_is_noop_when_feature_off` (`#[cfg(not(feature="daily_universe_fetcher"))]`) | stub returns; compiles without QuestDB |
| F4 | `test_groww_master_string_columns_are_ilp_sanitized` (storage) | both append fns: no `.symbol(...)`/`.column_str(...)` passes a raw `row.<field>` String without `sanitize_ilp_*` |
| F5 | `test_null_feed_row_distinct_from_groww_under_key` | `("",id,seg)`,`("dhan",id,seg)`,`("groww",id,seg)` are 3 distinct HashSet keys |
| F6 | `test_groww_ist_midnight_matches_dhan_lifecycle_convention` + `test_groww_constituency_index_name_is_ntm_pinned` | Groww nanos == Dhan `date_naive().and_hms_opt(0,0,0).and_utc()` nanos for fixed dates; NTM index_name pinned |

Verify: `cargo fmt --check`; scoped `cargo test -p tickvault-core --lib
feed::groww::shared_master_writer` WITH and WITHOUT the feature; `cargo test -p
tickvault-storage groww_master_ilp_sanitize_guard --features daily_universe_fetcher`;
`cargo clippy -p tickvault-core -p tickvault-storage --features daily_universe_fetcher
-- -D warnings -W clippy::perf`; `cargo build --workspace --features daily_universe_fetcher`.

## Rollback

Pure test-only + two tiny pure helper fns + one comment. Revert the single commit
to restore the exact pre-PR behaviour — there is no runtime/data-path change, no
schema change, no config change. The helpers are referenced by `persist_groww_instruments`
(so they have call sites; pub-fn-wiring stays green) and by tests.

## Observability

No new metric/log/Telegram/audit surface is added — the existing
`tv_groww_master_persist_errors_total{stage}` counter + `error!(code=GROWW-MASTER-01)`
(runbook `groww-shared-master-error-codes.md`) are now ratcheted by F2's
source-scan. F4 adds a build-failing ILP-injection guard (security hardening
layer). All other items are regression ratchets that fail the build on drift.

---

## Per-Item Guarantee Matrix

### 15-row "100% everything" matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | new unit tests cover both pure helpers + builders | `cargo test -p tickvault-core` | tests added in this PR |
| 100% audit coverage | reuses `instrument_lifecycle`/`index_constituency` + `*_audit`; no new event | `questdb_sql` | N/A — no new event type |
| 100% testing coverage | categories 1/2/3/4/13/19/20 (smoke/happy/error/edge/panic-safety/security/dedup) | `cargo test --lib` green | item declares categories above |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan | pre-push mandatory | all gates green |
| 100% code performance | pure helpers are O(1), zero-alloc, NOT a hot path | N/A — cold-path master writer | no DHAT needed (cold path) |
| 100% monitoring | existing `tv_groww_master_persist_errors_total{stage}` | `run_doctor` | F2 ratchets the counter site |
| 100% logging | `error!(code=GROWW-MASTER-01)` on each failure arm | errors.jsonl | F2 source-scan pins it |
| 100% alerting | GROWW-MASTER-01 → Telegram via 5-sink | `run_doctor` | existing alert path, ratcheted |
| 100% security | F4 ILP-injection source-scan guard + secret-scan | `cargo audit` | F4 adds attack-surface ratchet |
| 100% security hardening | `sanitize_ilp_*` on every Groww string column (ratcheted) | build gate | F4 |
| 100% bugs fixing | 6 audited gaps closed with ratchets | this PR | F1–F6 |
| 100% scenarios covering | dry-run / degrade / feature-off / raw-write / brownfield / IST-midnight | `cargo test` | F1–F6 enumerated |
| 100% functionalities covering | every new pub/priv fn has a test AND a call site | pre-push gates 6+11 | helpers wired into persist + tested |
| 100% code review | adversarial review on the diff | per-PR | orchestrator review (no push) |
| 100% extreme check | every item is a ratchet that fails the build on regression | every commit | F1–F6 are build-failing |

### 7-row "Resilience demand" matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | cold-path master writer is OFF the `ticks`/live path entirely | F1–F6 add no tick-drop path; `ticks` untouched |
| WS never disconnects | unrelated — no WS code touched | N/A |
| Never slow/locked/hanged | pure O(1) helpers, no alloc, no I/O | no hot-path allocation added |
| QuestDB never fails | degrade-safe: persist failure logs+counts+returns (F2) | F2 pins the no-abort behaviour |
| O(1) latency | helpers are O(1); builders are honestly O(n) over the daily set (cold path, flagged) | no per-tick path touched |
| Uniqueness + dedup | composite `(security_id, exchange_segment, feed)` (I-P1-11 + feed-in-key) | F5 pins feed-distinctness incl. legacy NULL |
| Real-time proof | existing counter + GROWW-MASTER-01 error code, now ratcheted | F2 + F4 source-scans |

## Plan Items

- [x] F1 — `should_skip_master_append` pure gate + `test_persist_dry_run_skips_append`
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_persist_dry_run_skips_append
- [x] F2 — `classify_persist_failure` pure fn + `test_persist_failure_classifies_groww_master_01_without_abort`
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_persist_failure_classifies_groww_master_01_without_abort
- [x] F3 — `test_persist_is_noop_when_feature_off`
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_persist_is_noop_when_feature_off
- [x] F4 — ILP-sanitize source-scan guard
  - Files: crates/storage/tests/groww_master_ilp_sanitize_guard.rs
  - Tests: test_groww_master_string_columns_are_ilp_sanitized
- [x] F5 — `test_null_feed_row_distinct_from_groww_under_key`
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_null_feed_row_distinct_from_groww_under_key
- [x] F6 — IST-midnight convention + NTM index_name pin
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_groww_ist_midnight_matches_dhan_lifecycle_convention, test_groww_constituency_index_name_is_ntm_pinned

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | dry_run=true, non-empty set | rows built (len>0), append SKIPPED |
| 2 | persist append fails (lifecycle/constituency) | log+count GROWW-MASTER-01, return, no panic |
| 3 | feature OFF | persist is a compiled no-op, no QuestDB symbol referenced |
| 4 | future raw Groww string write | build fails (F4 guard) |
| 5 | legacy feed=NULL row vs dhan/groww | 3 distinct composite keys |
| 6 | Groww date→nanos vs Dhan convention | exactly equal for every parseable date |
