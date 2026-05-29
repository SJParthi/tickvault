# Implementation Plan: Lifecycle/audit bulk writes → QuestDB ILP (proper industrial fix)

**Status:** VERIFIED
**Date:** 2026-05-29
**Approved by:** Parthiban ("yes — switch to ILP", "professional industrial standard ... with Z+ defensive layers")

## Problem (root cause, not symptom)

The `instrument_lifecycle` + `instrument_lifecycle_audit` BULK writers shoved
multi-row SQL `INSERT` through QuestDB's `/exec` endpoint — the SQL rides in the
**URL query string**, capped by QuestDB's request-header buffer. At the ~79K-row
F&O master this overflows (5000-row chunks → MB URL; #879's 32 KB → ~80 KB
encoded → still over; #880's 4 KB band-aid works but is thousands of tiny
round-trips). `/exec` is the **admin/query door**, never the bulk-ingest door.

## Fix — use ILP (the real ingestion pipe), exactly like ticks/candles

QuestDB's bulk path is **ILP on port 9009** (`Sender`/`Buffer`). The lifecycle +
audit tables are already `timestamp(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS`
— ILP writes to them cleanly and DEDUP is preserved (same as the ticks table).
No URL limit, one stream, ~1-2s for the whole master.

## Plan Items

- [x] Item 1 — `build_lifecycle_ilp_row(buffer, row)` (pure ILP-line builder; symbols-before-fields; empty SYMBOLs skipped → NULL; `finite_or_zero` f64; `sanitize_ilp_symbol` for symbol/str; `column_ts` nanos; `.at(designated_ts)`)
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs
  - Tests: test_build_lifecycle_ilp_row_content, test_build_lifecycle_ilp_row_empty_symbols_skipped_no_error
- [x] Item 2 — `build_lifecycle_audit_ilp_row(buffer, row)` (audit schema; empty `from_state` skipped)
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs
  - Tests: test_build_lifecycle_audit_ilp_row_content
- [x] Item 3 — rewrite `append_instrument_lifecycle_rows` + `append_instrument_lifecycle_audit_rows` to ILP: build whole `Buffer` (borrows rows), then `spawn_blocking` `Sender::from_conf(build_ilp_conf_string).flush()` (no blocking on the async runtime); Err on connect/flush → caller counts errors → idempotent next boot (Z+ L6)
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs
  - Tests: test_build_lifecycle_audit_ilp_row_preserves_field_deltas_json
- [x] Item 4 — remove now-dead `/exec` bulk path: `build_lifecycle_multi_insert_sql`, `build_lifecycle_audit_multi_insert_sql`, `build_size_bounded_inserts`, `LIFECYCLE_INSERT_BATCH_SIZE`, `LIFECYCLE_INSERT_MAX_SQL_BYTES` + their unit tests
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs
- [x] Item 5 — update guard ratchet to pin the ILP path (Sender/Buffer/.symbol/build_ilp_conf_string), drop the size-bound assertions
  - Files: crates/storage/tests/daily_universe_scope_guard.rs
  - Tests: lifecycle_persistence_uses_ilp_not_exec_url
- [x] Item 6 — fix stale module/doc comments (`/exec`, "PARTITION BY NONE") to reflect ILP + actual WAL DDL
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs

## Z+ defensive layers

| Layer | Mechanism |
|---|---|
| L1 DETECT | `Sender::from_conf` / `flush` returns `Err` → propagated |
| L2 VERIFY | `buffer.row_count()` asserted in unit tests; builder returns `Result` |
| L5 AUDIT | the `instrument_lifecycle_audit` table itself (this PR keeps it, on ILP) |
| L6 RECOVER | flush Err → apply_reconcile counts errors → fail-closed boot → idempotent re-run (DEDUP UPSERT KEYS make re-run safe) |
| L7 COOLDOWN | reconcile is once-per-boot cold path; warm-skip (#876) avoids it daily |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 79K-row master cold boot | one ILP stream, ~1-2s, boot completes |
| 2 | index row (empty exchange_id/option_type/underlying) | empty SYMBOLs skipped → NULL, no ILP error |
| 3 | audit `appeared` row (empty from_state) | from_state skipped → NULL |
| 4 | non-finite strike/tick | clamped to 0.0 (finite_or_zero) |
| 5 | QuestDB ILP down | flush Err → fail-closed → next boot retries (idempotent) |
| 6 | re-run same CSV | DEDUP UPSERT KEYS dedup; no dupes |

## Honest 100% claim

100% inside the tested envelope: bulk lifecycle/audit rows now ingest via ILP
(port 9009), the same pipe proven for millions of ticks/candles — no URL limit
at any master size. DEDUP UPSERT KEYS preserved (WAL tables). Flush failure is
fail-closed + idempotent-retryable. Builder unit-tested for content + empty
symbols + non-finite f64; guard ratchet pins the ILP path.
