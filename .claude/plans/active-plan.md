# Implementation Plan: Daily end-to-end tick-conservation audit (WAL vs DB) — TICK-CONSERVE-01

**Status:** APPROVED
**Date:** 2026-06-10
**Approved by:** Parthiban (verbatim, this session 2026-06-10: "Go ahead to achieve zero tick loss" — directly approving the proposed Future #1: "Daily end-to-end count audit: WAL disk-log frame count vs DB row count per day — one more independent 'not one tick unaccounted' number", following his "I suspect maybe in some cases we are removing ticks … or dropping … or duplicating … or missing … wal or buffer or spill or dlq or in memory ram or db … how do you guarantee this area").

**Per-item guarantee matrix:** cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` per its "or cross-reference it" clause. Deltas: ALL new code is COLD path (once-daily 15:40 IST scheduler + boot-time only) — zero per-tick hot-path change; no new dep (reqwest/questdb already workspace deps of app/storage); one new ErrorCode variant + rule file (cross-ref contract); one new audit table with DEDUP keys; no schema change to existing tables.

## Design

**The gap this closes:** the in-loop conservation ledger (60 s) proves
`processed == persisted + filtered` INSIDE the process, and the 15:31 IST
cross-verify proves candle-level agreement with Dhan — but nothing today
reconciles the THREE independent record stores end-to-end:
(1) the WAL disk log (every frame Dhan delivered, durably captured at the
socket), (2) the processor's outcome counters, (3) the actual queryable
`ticks` rows in QuestDB. This PR adds that daily reconciliation.

**Components:**

1. **WAL day-counting** (`crates/storage/src/ws_frame_spill.rs`):
   `pub fn count_frames_for_ist_day(wal_dir, ist_day_number) -> WalDayFrameCounts`.
   Scans live `*.wal` segments AND `archive/` (the boot replay moves processed
   segments there), read-only — never archives, never mutates. Per-frame day
   attribution via `frame_seq` (wall-nanos-seeded, strictly monotonic →
   nanos→IST day); per-frame classification via payload byte 0 (response
   code 2/4/8 = tick frame; others = non-tick). Legacy v1 records
   (frame_seq=0) count into an explicit `unattributable` bucket — never
   silently miscounted. Segment pre-filter by filename nanos (skip segments
   created before yesterday) keeps the scan O(today's data).

2. **Audit table** (`crates/storage/src/tick_conservation_audit_persistence.rs`):
   `tick_conservation_audit` — one row per run: ts, trading_date_ist,
   wal_tick_frames, wal_other_frames, wal_unattributable, db_rows,
   processed, persisted, junk, stale_day, outside_hours, dedup,
   parse_errors, storage_errors, dropped_total, delivery_residual,
   outcome_residual, partial_coverage BOOLEAN, outcome SYMBOL
   ('balanced' / 'leak' / 'partial'). `DEDUP UPSERT KEYS(ts, trading_date_ist)`
   (designated timestamp in key per the 2026-04-28 regression rule; the
   table is per-run, not per-instrument, so no security_id — I-P1-11 N/A).
   ILP writer mirroring `cross_verify_1m_audit_persistence.rs`.

3. **ErrorCode** (`crates/common/src/error_code.rs`):
   `TickConserve01DailyResidual` → `"TICK-CONSERVE-01"`, Severity::High,
   NOT auto-triage-safe. Runbook: new rule file
   `.claude/rules/project/tick-conservation-audit-error-codes.md`
   (satisfies the cross-ref test both directions).

4. **Scheduler + runner** (`crates/app/src/tick_conservation_boot.rs`,
   mirroring `cross_verify_1m_boot.rs`):
   - `decide_conservation_start(now_ist_secs_of_day, is_trading_day, force)`
     pure fn — fire at **15:40:00 IST** (after the 15:31 cross-verify and the
     post-close flushes), skip on non-trading days, skip when boot is past
     15:40 unless forced.
   - `run_tick_conservation_audit(...)`: (a) count WAL frames for today;
     (b) self-scrape `http://127.0.0.1:{metrics_port}/metrics` and parse the
     7 outcome counters (pure `parse_prom_counter(body, name)`);
     (c) QuestDB `/exec` `select count() from ticks` windowed to today's IST
     day; (d) compute `delivery_residual = wal_tick_frames − processed` and
     `outcome_residual = processed − (persisted + junk + stale_day +
     outside_hours + dedup + storage_errors)` (pure fn); (e) classify
     (pure fn): `partial` when the process booted after 09:00 IST (counters
     don't cover the whole session) — flagged, never camouflaged;
     `leak` when either residual > 0; else `balanced`;
     (f) emit `info!` (balanced) or `error!(code = TICK-CONSERVE-01)`
     (leak → Telegram via the 5-sink chain); (g) append the audit row.
   - Spawn site in `main.rs` next to the cross-verify spawn.

**Honest residual semantics (no illusion):** a positive `delivery_residual`
usually means frames are in the WAL but not yet processed (live-path
backpressure drop) — they replay at next boot; the audit row pins the exact
count instead of letting it hide. A positive `outcome_residual` is a true
in-process leak (the same condition the 60 s ledger pages on). `db_rows`
is reported alongside but NOT in the residual identity: QuestDB DEDUP can
legitimately collapse replayed duplicates and a mid-day restart legitimately
makes db_rows > persisted-since-boot — the row records both numbers so the
operator sees the relationship without a false alarm.

## Edge Cases

- **Mid-day restart**: counters reset at boot → outcome identity only covers
  since-boot. Detected via boot-time captured at spawn; row gets
  `partial_coverage=true`, outcome `partial` (never a false "balanced").
- **v1 legacy WAL records** (frame_seq=0): counted as `unattributable`,
  excluded from the residual identity, visible in the row.
- **Non-trading day / market closed boot**: scheduler skips (audit Rule 3).
- **QuestDB unreachable at 15:40**: db_rows=None → row written with
  outcome `partial` + warn; never blocks, never fakes a zero.
- **Metrics endpoint disabled** (config): counters unavailable → `partial`.
- **WAL dir missing/empty** (fresh box): zero counts, `balanced` only if
  processed also 0; else arithmetic stands on its own.
- **Frames captured 15:30–15:40** (post-close stragglers): attributed to
  today by frame_seq day — they are outside_hours-filtered, so the outcome
  identity still balances.
- **Archive scan cost**: filename-nanos pre-filter bounds the scan to
  yesterday+today segments regardless of archive age.

## Failure Modes

- Audit task panics → it is a spawned cold task; failure logs `error!` and
  the day simply lacks a row — the absence is visible in the audit table
  trend (and the in-loop 60 s ledger still runs independently).
- Self-scrape parse drift (exporter format change) → parse returns None →
  `partial` outcome; pure-fn unit tests pin the current format.
- WAL segment corrupted → the counting scan reuses the replay parser's
  boundary-stop semantics; corrupted tail counted segments logged, run
  continues (mirrors `replay_all`).
- False-leak risk at 15:40 from in-flight ticks → trigger is 10 min after
  close; ring is drained by the post-close flush; if a flush is genuinely
  stuck, the leak alert is CORRECT, not false.

## Test Plan

- storage: unit tests for `count_frames_for_ist_day` (tick/non-tick
  classification, day attribution, v1-unattributable, archive+live merge,
  filename pre-filter) + DDL/DEDUP tests for the new audit table
  (`test_conservation_ddl_contains_expected_columns`,
  `test_conservation_dedup_key_includes_designated_timestamp`).
- app: pure-fn tests for `decide_conservation_start` (before/at/after
  15:40, non-trading-day, force), `parse_prom_counter`,
  `compute_residuals`, `classify_outcome` (balanced/leak/partial).
- common: ErrorCode suite auto-covers the new variant (uniqueness,
  roundtrip, runbook path exists, severity) + cross-ref test requires the
  new rule file — `cargo test --workspace` (common changed → escalation per
  testing-scope rule).

## Rollback

- Single PR; revert = `git revert` of the squash commit. The audit table is
  additive (CREATE IF NOT EXISTS; orphaned harmlessly on revert). The
  ErrorCode variant removal is safe post-revert (no other emit sites). No
  hot-path or existing-table change anywhere.

## Observability

- New audit table `tick_conservation_audit` (the daily proof ledger).
- New ErrorCode TICK-CONSERVE-01 (High) → Telegram on leak via the 5-sink
  chain; balanced days emit `info!` with the full numbers.
- New counter `tv_tick_conservation_audit_runs_total{outcome}` so the
  dashboard can show the audit is alive.
- Runbook: `.claude/rules/project/tick-conservation-audit-error-codes.md`.

## Plan Items

- [ ] Item 1 — WAL day-count scan (read-only) in ws_frame_spill
  - Files: crates/storage/src/ws_frame_spill.rs
  - Tests: test_count_frames_for_ist_day_classifies_tick_codes, test_count_frames_day_attribution_and_v1_unattributable, test_count_frames_scans_archive_dir

- [ ] Item 2 — tick_conservation_audit table + ILP writer
  - Files: crates/storage/src/tick_conservation_audit_persistence.rs, crates/storage/src/lib.rs
  - Tests: test_conservation_ddl_contains_expected_columns, test_conservation_dedup_key_includes_designated_timestamp

- [ ] Item 3 — ErrorCode TICK-CONSERVE-01 + runbook rule file
  - Files: crates/common/src/error_code.rs, .claude/rules/project/tick-conservation-audit-error-codes.md
  - Tests: test_all_variants_have_unique_code_str

- [ ] Item 4 — scheduler + runner + main.rs spawn
  - Files: crates/app/src/tick_conservation_boot.rs, crates/app/src/lib.rs, crates/app/src/main.rs
  - Tests: test_decide_conservation_start_before_after_force, test_parse_prom_counter, test_compute_residuals, test_classify_outcome_partial_leak_balanced

- [ ] Item 5 — workspace verification + 3-agent review + PR to merge
  - Files: (verification — PR body)
  - Tests: test_classify_outcome_partial_leak_balanced

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy full day | row: residuals 0, outcome balanced, info! line |
| 2 | Burst dropped N frames from live path | delivery_residual = N → TICK-CONSERVE-01 + exact count; frames replay next boot |
| 3 | Mid-day restart | partial_coverage=true, outcome partial — honest, no false alarm |
| 4 | QuestDB down at 15:40 | row with db_rows missing, outcome partial, warn |
| 5 | Non-trading day | scheduler skips, no row |
