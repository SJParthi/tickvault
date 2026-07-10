# Implementation Plan: Groww daily tick-conservation audit (feed='groww')

**Status:** VERIFIED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — "go ahead fully" (2026-07-01, before sleep) + standing 100%-audit-coverage directive. Closes the HIGH finding from the 2026-07-01 both-feeds adversarial hunt: `tick_conservation_boot.rs:424` hardcodes `feed: CONSERVATION_FEED_DHAN` and reconciles ONLY the Dhan WAL, so the Groww lane has ZERO conservation reconciliation despite the `tick_conservation_audit` table being feed-keyed.

## Design

Add a Groww daily conservation audit that mirrors the Dhan one's philosophy — an
on-disk delivered-count vs the persisted DB rows — but with Groww's own ground
truth:

- **Delivered (ground truth):** the Groww sidecar NDJSON file
  (`data/groww/live-ticks.ndjson`) is the durable capture floor (fsync'd per
  callback). A new PURE, unit-tested `count_groww_ndjson_lines_for_ist_day(path,
  target_ist_day) -> u64` scans the file and counts lines whose `ts_ist_nanos`
  floors to the target IST day. This mirrors Dhan's `count_frames_for_ist_day`
  (on-disk, survives restart).
- **Persisted:** `select count() from ticks where feed='groww' and ts >= day_start
  and ts < day_end` (feed-filtered — the existing Dhan query is NOT feed-filtered).
- **Residual:** `delivery_residual = ndjson_lines − groww_db_rows`. Written as a
  `TickConservationRow` with `feed = CONSERVATION_FEED_GROWW` into the SAME
  `tick_conservation_audit` table (DEDUP `(ts, trading_date_ist, feed)` — the Dhan
  and Groww rows coexist by feed).

**Honest classification (no false CRITICAL):** a POSITIVE residual is expected for
BENIGN reasons on the Groww lane — (a) the in-flight NDJSON tail not yet drained at
15:40, and (b) DEDUP collapse of lines re-tailed from the persisted byte-offset
after a same-day restart (the sidecar file is append-only; the bridge re-reads from
`offset`, and a re-persisted line with the same monotonic `capture_seq` collapses in
QuestDB → `db_rows < ndjson_lines`). So the Groww audit classifies a positive
residual as **PARTIAL (diagnostic)**, NEVER a leak-class CRITICAL page. It emits an
INFO-level forensic row + the numbers every trading day (that IS the 100%-audit
coverage the charter demands); the leak-alarm THRESHOLD precision is on-box-tuned
later (like the Dhan audit was validated at 15:40 IST on a real box). This avoids
the false-loss-alarm that a naive `residual>0 ⇒ leak` would cause.

Also fix the LOW: the stale DEDUP-key doc comment in
`tick_conservation_audit_persistence.rs:40` (shows the pre-`feed` key).

## Edge Cases
- NDJSON file absent (Groww disabled / first boot) → line count 0 → sources
  incomplete → PARTIAL (never a leak). No panic.
- Malformed / half-written last line → skipped (parse the `ts_ist_nanos` field
  defensively; a line without it is not counted, never panics).
- Lines from OTHER IST days in the same append-only file → excluded by the
  ts-in-day floor (both the counter and the DB query are day-scoped → consistent).
- QuestDB unreachable → `db_rows = -1` → sources incomplete → PARTIAL.
- Groww feed OFF that day → 0 ndjson lines, 0 db rows → balanced/partial, no page.
- Positive residual (tail / DEDUP-collapse) → PARTIAL diagnostic, NOT a CRITICAL.

## Failure Modes
- Any I/O or query failure degrades to PARTIAL + a WARN — never a panic, never a
  false leak. The audit is best-effort forensic (same contract as the Dhan one).
- The whole run is `spawn_blocking` for the file scan (off the async worker) +
  bounded QuestDB HTTP; a hang cannot block the runtime.

## Test Plan
- Unit (pure, offline-verifiable): `count_groww_ndjson_lines_for_ist_day` on a
  temp NDJSON — lines in-day counted, out-of-day excluded, malformed skipped,
  missing file → 0. `test_groww_ndjson_line_count_*`.
- Classification: a positive Groww residual classifies PARTIAL (not Leak) —
  `test_groww_conservation_positive_residual_is_partial_not_leak`.
- Ratchet: source-scan that the Groww conservation audit is wired into `main.rs`
  at the 15:40 scheduler — `crates/core/src/auth/secret_manager.rs`-style wiring
  guard OR a test in the boot module. `test_groww_conservation_audit_is_scheduled`.
- `cargo test -p tickvault-app tick_conservation`, `cargo build -p tickvault-app`,
  `cargo build -p tickvault-storage`.

## Rollback
`git revert` — additive: a new const, a new pure counter + run fn, one wiring call
at 15:40, a doc-comment fix, and tests. No schema change (the table + feed-keyed
DEDUP already exist). Reverting removes the Groww row emission; the Dhan audit is
untouched.

## Observability
- New `feed='groww'` rows in `tick_conservation_audit` (queryable forensic).
- `tv_tick_conservation_audit_runs_total{outcome, feed}` — the Groww run
  increments with `feed="groww"` so the two lanes are distinguishable.
- TICK-CONSERVE-01 remains the code; the Groww run emits it only on a
  sources-complete, egregiously-large residual (on-box-tuned), else INFO.

## Plan Items
- [x] Add `CONSERVATION_FEED_GROWW` const + fix stale DEDUP doc comment
  - Files: crates/storage/src/tick_conservation_audit_persistence.rs
  - Tests: (const used by the boot tests below)
- [x] Add `count_groww_ndjson_lines_for_ist_day` pure fn + `run_groww_tick_conservation_audit`
  - Files: crates/app/src/tick_conservation_boot.rs
  - Tests: test_count_groww_ndjson_lines_for_ist_day_in_day, test_groww_ndjson_line_count_excludes_other_day, test_groww_ndjson_line_count_skips_malformed_and_blank, test_groww_ndjson_line_count_missing_file_is_zero, test_classify_groww_outcome_positive_residual_is_partial_not_leak, test_parse_groww_line_ts_extracts_and_filters
- [x] Wire the Groww run at 15:40 IST right after the Dhan run
  - Files: crates/app/src/main.rs, crates/app/tests/tick_conservation_wiring_guard.rs
  - Tests: test_dhan_tick_conservation_audit_is_wired_into_main, test_groww_tick_conservation_audit_is_wired_into_main, test_groww_conservation_is_runtime_gated_and_shares_the_dhan_window

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 1000 ndjson lines, 1000 groww db rows | residual 0 → balanced |
| 2 | 1000 lines, 990 db rows (DEDUP collapse / tail) | residual 10 → PARTIAL diagnostic, NO page |
| 3 | NDJSON absent (Groww off) | 0/0 → partial, no page |
| 4 | QuestDB down | db_rows −1 → partial, WARN |
| 5 | malformed last line | skipped, count correct, no panic |
