# Implementation Plan: Operator portal "Wipe GROWW only" — surgical per-feed wipe

**Status:** APPROVED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — verbatim demand this day: "why the fuck these are not yet deleted... groww data" (groww rows must be deletable without touching dhan data or the SEBI audit tables).
**Branch:** `claude/charming-newton-qy6j0i`
**Changed area:** deploy-only (`deploy/aws/lambda/operator-control/handler.py` + `test_handler.py`). No `crates/` code.
**Guarantee matrices:** cross-referenced per `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices apply; deploy-only rows are N/A'd honestly in the PR body).

---

## Plan Items

- [x] New Lambda action `wipe-groww` (guards: `_DESTRUCTIVE` market-hours block unless force, server-side confirm token `GROWW`, box-must-be-RUNNING check) + pure command builder `_wipe_groww_commands()` + embedded `_GROWW_WIPE_PY` per-table rewrite
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_wipe_groww_is_in_destructive_set, test_wipe_groww_without_confirm_token_is_blocked, test_wipe_groww_requires_running_box, test_wipe_groww_forced_is_surgical_and_scoped, test_wipe_groww_never_drops_original_before_verify
- [x] Portal UI: "🧹 Wipe GROWW data only" button in CONTROL (type GROWW confirm, honest explanatory line: dhan untouched, audit tables kept per SEBI) + result polling via the existing command-status poller
  - Files: deploy/aws/lambda/operator-control/handler.py (`_console_html`)
  - Tests: test_html_has_wipe_groww_button, test_poll_recognises_groww_wipe_markers
- [x] Unit tests green: `python3 -m unittest test_handler -v`
  - Files: deploy/aws/lambda/operator-control/test_handler.py
  - Tests: (all above)

## Design

QuestDB cannot DELETE rows and groww+dhan share partitions, so groww-only
removal is a **per-table rewrite**, mirroring the resurrect-proof pattern of
the existing `wipe-questdb` action (PYWIPE block) but surgical:

1. Stop the tickvault app + `pkill -f groww_sidecar.py` (releases writers,
   stops the capture-file appender + bridge).
2. `rm -rf /opt/tickvault/data/groww` — the PROVEN resurrection vector
   (capture file `live-ticks.ndjson`, `groww-status.json`, the bridge
   flushed-offset snapshot) — removed BEFORE restart. Dhan's replay sources
   (`data/ws_wal`, `spill`, `dlq`, `instrument-cache`) are deliberately
   UNTOUCHED (they carry dhan data).
3. Embedded python (PYGROWW heredoc, same mechanism as PYWIPE): for `ticks`
   and every dynamically-discovered `candles_*` table
   (`SELECT table_name FROM tables()`):
   - pre-check the LIVE column set == the canonical column set (mirrored
     EXACTLY from `crates/storage/src/tick_persistence.rs::TICKS_CREATE_DDL`
     19 cols / `crates/storage/src/shadow_persistence.rs` 15 cols); skip on
     drift — never guess a schema;
   - `CREATE TABLE <t>_gwipe` with the exact DDL (designated `ts`,
     `PARTITION BY HOUR WAL` for ticks / `PARTITION BY DAY` + in-DDL DEDUP for
     candles) + `DEDUP ENABLE UPSERT KEYS(ts, security_id, segment,
     capture_seq, feed)` for ticks;
   - `INSERT INTO <t>_gwipe (cols) SELECT cols FROM <t> WHERE feed != 'groww'
     OR feed IS NULL` (explicit column list — immune to live column-order
     drift; NULL-feed legacy rows are dhan rows and are KEPT);
   - VERIFY (WAL apply is async — poll up to 120 s) that
     `count(new) == count(old) - count(old WHERE feed='groww')` BEFORE any
     DROP; on mismatch: drop the temp table, keep the original, mark PARTIAL;
   - only then `DROP TABLE <t>` + `RENAME TABLE <t>_gwipe TO <t>`.
4. Verify groww counts are 0 (ticks + candles_1m) while the app is still
   stopped (no live-write race), THEN restart the app (feeds config
   unchanged).
5. Print `GROWW-WIPE-COMPLETE` only when the table stage reported OK AND both
   counts are 0; otherwise `GROWW-WIPE-PARTIAL` with the per-table GWIPE
   lines as reasons. Per-table before/groww-removed/after counts are echoed.

Lambda guards mirror `wipe-questdb`: member of `_DESTRUCTIVE` (blocked
09:15–15:30 IST Mon–Fri unless force), server-side confirm token
`{"confirm": "GROWW"}` (a UI-bypass cannot fire it accidentally), and a
box-must-be-RUNNING `describe_instances` check (the rewrite needs QuestDB +
SSM live).

SCOPE LOCK: market-data tables ONLY (`ticks` + `candles_*`).
`instrument_lifecycle`, `instrument_lifecycle_audit`, `ws_event_audit`,
`index_constituency`, `tick_conservation_audit`, `prev_day_ohlcv` and every
`*_audit` table are SEBI never-delete — not in the target filter.

## Edge Cases

- Live table column ORDER differs from the canonical CREATE (older table +
  `ALTER ADD COLUMN` appends at the end) → explicit column lists in the
  INSERT make the copy order-independent; a column-SET mismatch skips the
  table (PARTIAL) instead of silently mis-mapping.
- `feed IS NULL` legacy rows → kept (they are pre-feed dhan rows; `feed !=
  'groww'` alone would drop them under SQL NULL semantics).
- Leftover `<t>_gwipe` from a previous crashed run → `DROP TABLE IF EXISTS`
  before CREATE; discovery filter excludes `*_gwipe` names.
- A table with zero groww rows → no rewrite at all (`GWIPE-CLEAN`).
- No tables found at all → honest PARTIAL (never a fake COMPLETE on an empty
  compare set — audit Rule 11).
- Market hours → 409 unless force checkbox; box stopped → 409 with the state.

## Failure Modes

- INSERT fails / count never converges (WAL apply stall, disk full) →
  GWIPE-ABORT: original table untouched, temp dropped, run ends PARTIAL.
- DROP old succeeded but RENAME failed (worst case) → GWIPE-CRITICAL line
  names the safe copy `<t>_gwipe` and prints the exact manual
  `RENAME TABLE` to run; the app restart recreates an empty `<t>` via boot
  DDL, so nothing crashes — the dhan rows are safe in `<t>_gwipe`.
- SSM 1-hour default execution timeout on very large tables → un-rewritten
  tables remain fully intact (originals are never dropped before verify);
  the run reads PARTIAL. Documented for the operator.
- Groww rows sitting in `data/spill`/`dlq` at wipe time (only possible if
  QuestDB was down) would re-drain after restart — honest known limit; the
  app is stopped first so the window is effectively empty.

## Test Plan

`cd deploy/aws/lambda/operator-control && python3 -m unittest test_handler -v`
— new tests assert: destructive-set membership, confirm-token 409, stopped-box
409, captured SSM commands are surgical (no TRUNCATE, no ws_wal/spill/dlq
removal, exact DDL + DEDUP keys, dynamic candles_* discovery, keep-filter with
NULL handling, SEBI tables absent), verify-before-DROP ordering inside
`_GROWW_WIPE_PY`, and the UI button + poll markers. All pre-existing tests stay
green.

## Rollback

Single revert of the one commit restores the previous handler + tests; the
Lambda redeploys from main via the existing deploy workflow. No schema, no
Rust, no config migration involved. Mid-run rollback safety is inherent:
originals are never dropped before the copy is verified.

## Observability

- Every stage echoes labeled lines into the SSM command output
  (`GWIPE-TARGETS`, per-table `GWIPE-OK/CLEAN/SKIP/ABORT/CRITICAL` with
  before/groww_removed/after counts, `GROWW-WIPE-RESULT`, final
  `GROWW-WIPE-COMPLETE`/`GROWW-WIPE-PARTIAL`).
- The UI polls the REAL outcome via the existing `command-status` action
  (never claims success at dispatch — no false-OK).
- Full python transcript persisted on the box at `/tmp/tv-gwipe.log` for
  forensics.
