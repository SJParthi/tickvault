# Implementation Plan: Groww instrument_lifecycle_audit transition chain (feed='groww')

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — 100%-audit-coverage charter directive + his direct 2026-06-29 question about the empty `instrument_lifecycle_audit WHERE feed='groww'` table.

## Design

The Groww shared-master writer (`crates/core/src/feed/groww/shared_master_writer.rs::persist_groww_instruments`)
today UPSERTs `instrument_lifecycle` (feed='groww', ~767 rows) + `index_constituency` (~742) but emits
ZERO `instrument_lifecycle_audit` transition rows. The Dhan side writes the audit chain via
`crates/app/src/apply_reconcile_plan.rs` (reusing the pure `tickvault_storage::lifecycle_reconciler::classify_transition`).
This closes the gap for Groww by mirroring that pattern, scoped to `crates/core` (no core→app dependency).

New code (all in `shared_master_writer.rs`, feature-gated `daily_universe_fetcher`):
1. `GrowwPriorAttrs` + `GrowwLifecycleKey = (i64, String)` — the prior `feed='groww'` snapshot shape.
2. `build_groww_lifecycle_select_sql()` — pure SELECT of the prior `feed='groww'` rows (`WHERE feed='groww' AND dry_run=false`).
3. `parse_groww_lifecycle_dataset(&Value)` — pure parser (mirrors `lifecycle_cache_loader::parse_questdb_lifecycle_dataset`).
4. `groww_today_attrs(&GrowwWatchSet)` — pure: master_entries → `Vec<(key, instrument_type, lot, tick, symbol)>` today attrs.
5. `build_groww_audit_rows(prior, today, now_ist_nanos, today_ist_nanos)` — PURE diff classifier reusing
   `classify_transition` + `ReconcileInput` (storage), producing `Vec<OwnedGrowwAuditRow>` (appeared/updated/expired/etc).
   Day-1 (empty prior) → every row `appeared`, no expired pass. Composite `(security_id, exchange_segment)` per I-P1-11.
6. `load_prev_groww_lifecycle(&QuestDbConfig)` — thin async reqwest GET (TEST-EXEMPT; pure parts tested).
7. Wire into `persist_groww_instruments` BEFORE the lifecycle UPSERT, audit-first (§24): load prior → diff →
   `append_instrument_lifecycle_audit_rows` (existing storage fn) → existing UPSERT. dry_run skips append (§27).

## Edge Cases
- Day-1 / empty prior snapshot → all `appeared`, no expired pass (matches Dhan bootstrap).
- Unchanged instrument (present both days, no field change) → NO audit row.
- Disappeared instrument (in prior, absent today) → `expired` (or `segment_moved`).
- Locked prior row → no auto-flip (`classify_transition` returns None).
- dry_run=true → rows BUILT but NO append (isolation, §27).
- Malformed/unknown-state prior rows → counted, not guessed (parser).
- QuestDB unreachable at load → `Err` → log GROWW-MASTER-01, build NO audit rows, continue to data UPSERT.

## Failure Modes
- Audit load/append failure is best-effort, degrade-safe: log `error!(code=GROWW-MASTER-01)` + bump
  `tv_groww_master_persist_errors_total{stage="lifecycle_audit"}`, RETURN — NEVER abort Groww activation,
  the live feed, or any order/tick path. Reuses the existing `GROWW-MASTER-01` ErrorCode (shared-master persist failures).
- Next idempotent boot re-runs (DEDUP UPSERT KEYS make audit rows idempotent).

## Test Plan
- `crates/core/src/feed/groww/shared_master_writer.rs` unit tests (feature-gated):
  - diff: appeared (empty prior), updated (symbol/lot change), expired (disappeared), unchanged→no row, reactivated, day-1 all-appeared.
  - `parse_groww_lifecycle_dataset`: well-formed, empty, malformed, unknown-state-counted.
  - all rows tagged `feed='groww'`; composite-key correctness; IST-midnight nanos convention.
  - degrade-safe source-scan: audit failure arm logs+counts+returns (no panic/abort).
- Run `cargo test -p tickvault-core groww` (+ `--features daily_universe_fetcher`).
- Live diff-vs-yesterday needs real QuestDB with prior rows — NOT unit-verifiable here (honest); pure diff fn covers logic.

## Rollback
- Feature-gated under `daily_universe_fetcher`; default build unaffected. Revert the single commit. No schema change
  (table + columns + DEDUP key already exist with `feed` in-key). No data migration.

## Observability
- Audit rows land in `instrument_lifecycle_audit` (feed='groww') — queryable forensic chain (Layer 6 audit table).
- Failure → `error!(code=GROWW-MASTER-01)` (Layer 4 log → Telegram) + `tv_groww_master_persist_errors_total{stage}` (Layer 1 counter).
- Success → `info!` with audit-row count. Runbook: `.claude/rules/project/groww-shared-master-error-codes.md` (updated).

## Plan Items
- [x] Add pure diff + parser + builders + audit-row build to `shared_master_writer.rs` — Files: crates/core/src/feed/groww/shared_master_writer.rs — Tests: test_build_groww_audit_rows_*, test_parse_groww_lifecycle_dataset_*
- [x] Wire audit emission into `persist_groww_instruments` (audit-first, degrade-safe) — Files: crates/core/src/feed/groww/shared_master_writer.rs — Tests: source-scan degrade test
- [x] Update runbook + scope rule — Files: .claude/rules/project/groww-shared-master-error-codes.md, .claude/rules/project/groww-second-feed-scope-2026-06-19.md

## Per-Item Guarantee Matrix (15-row + 7-row) — cross-ref `.claude/rules/project/per-wave-guarantee-matrix.md`

### 15-row 100% matrix
| Demand | Proof for this item |
|---|---|
| 100% code coverage | pure diff/parser/builder fns unit-tested; async I/O leg TEST-EXEMPT (live QuestDB) |
| 100% audit coverage | THIS IS the audit-coverage fix — emits `instrument_lifecycle_audit` feed='groww' rows |
| 100% testing coverage | unit (diff/parser) + source-scan (degrade-safe) |
| 100% code checks | cargo check + fmt + banned-pattern + pub-fn-test guards |
| 100% code performance | cold-path daily diff; O(1) per-instrument HashMap lookup; flagged O(n) over ~767 (NOT hot path) |
| 100% monitoring | `tv_groww_master_persist_errors_total{stage="lifecycle_audit"}` counter |
| 100% logging | `error!(code=GROWW-MASTER-01)` on failure; `info!` row-count on success |
| 100% alerting | GROWW-MASTER-01 routes High→Telegram (existing) |
| 100% security | sanitize_audit_string on all CSV-derived strings; no secrets logged |
| 100% security hardening | reuses sanitized writer path; no new attack surface |
| 100% bugs fixing | adversarial 3-agent review (hot-path + security + hostile) before+after |
| 100% scenarios covering | day-1, unchanged, appeared, updated, expired, reactivated, dry-run, QuestDB-down |
| 100% functionalities covering | every new pub/priv fn has a test or TEST-EXEMPT + a call site |
| 100% code review | 3-agent pass on the diff |
| 100% extreme check | source-scan ratchet pins degrade-safe failure arm |

### 7-row resilience matrix
| Demand | Proof |
|---|---|
| Zero ticks lost | no tick path touched — cold-path master/audit only |
| WS never disconnects | n/a — no WS code |
| Never slow/locked/hanged | cold-path daily; no hot-path alloc added |
| QuestDB never fails | audit write is best-effort; failure absorbed (log+count+return), next boot re-runs |
| O(1) latency | O(1) per-instrument prior-map lookup; whole diff is O(n) cold-path (flagged honestly) |
| Uniqueness + dedup | composite `(security_id, exchange_segment, feed)` + audit DEDUP key incl transition_kind |
| Real-time proof | counter + log + queryable audit table |

## Honest 100% claim
"100% inside the tested envelope, with ratcheted regression coverage: the pure diff classifier
(appeared/updated/expired/reactivated/unchanged + day-1 bootstrap) is unit-tested; audit emission is best-effort
cold-path (failure logs GROWW-MASTER-01 + counts + returns, feed/ticks unaffected, next boot re-runs idempotently
via DEDUP UPSERT KEYS); composite-key `(security_id, exchange_segment, feed)` uniqueness. The live
diff-vs-yesterday against a real QuestDB carrying prior-day rows is NOT unit-verifiable in this environment —
the pure diff function carries the logic coverage; the I/O leg is TEST-EXEMPT and boot-integration-exercised."

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | Day-1, empty prior | every master entry → `appeared` audit row; no expired pass |
| 2 | Instrument unchanged | no audit row |
| 3 | Symbol/lot changed | `updated` audit row with the new attrs |
| 4 | Instrument disappeared | `expired` audit row carrying prior attrs |
| 5 | dry_run=true | rows built, NO append |
| 6 | QuestDB unreachable at load | Err → log GROWW-MASTER-01, no audit rows, data UPSERT still attempted |
