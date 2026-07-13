# Implementation Plan: Daily timeframe-consistency verifier (TF-VERIFY-01/02)

**Status:** APPROVED
**Date:** 2026-07-13
**Approved by:** Parthiban (operator) — 2026-07-13 timeframe-correctness directive relayed via the coordinator session

Operator directive (2026-07-13, verbatim): *"how will you guarantee that all
our defined timeframes internally are correct — how do you identify whether
any miscalculation or data issues"*.

Per-wave guarantee matrices: cross-reference per-wave-guarantee-matrix.md (15-row + 7-row matrices apply to this item).

Honest 100% claim: 100% inside the tested envelope, with ratcheted regression
coverage — every sealed higher-timeframe candle (2m..4h, both feeds) is
recomputed daily from the same day's stored 1-minute candles and compared
exactly (integer-paise OHLC, exact i64 volume), with truncation-tripwired
bounded queries (explicit LIMIT on every SELECT), a feed-keyed DEDUP audit
trail (`tf_consistency_audit`), a 10,000-row audit cap, a 900s wall-clock
budget, and a BLIND-honest verdict (zero-compared never reads as pass). NOT
claimed: a bug corrupting 1m and higher TFs identically over the same tick
stream is invisible here (the 15:31 Dhan REST cross-verify + the BruteX
comparison anchor the 1m base); upstream ticks the feeds never delivered are
out of scope; restart-window divergences are REPORTED, never auto-classified;
tick_count equality is soft (documented legitimate divergence classes).

## Plan Items

- [x] ErrorCode variants `TfVerify01MismatchFound` (TF-VERIFY-01, High,
  auto-triage NO via the severity-independent override list — the Futidx02
  precedent) + `TfVerify02RunDegraded` (TF-VERIFY-02, High, auto-triage yes)
  in the `common` crate, with the new `TF-VERIFY-` prefix whitelisted.
  - Files: crates/common/src/error_code.rs
  - Tests: test_code_str_follows_expected_prefix_pattern,
    test_tf_verify_codes_contract
- [x] `TfConsistencyConfig` (`[tf_consistency]`, `#[serde(default)] enabled`,
  Default OFF) wired into `ApplicationConfig` in the `common` crate;
  config/base.toml opts in with a dated comment.
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_tf_consistency_config_default_off,
    test_tf_consistency_config_absent_section_disabled,
    test_tf_consistency_config_explicit_enable
- [x] NotificationEvents `TfConsistencySummary` (data-dependent severity;
  BLIND / no_data wordings; ≤10 plain-English top_detail lines) +
  `TfConsistencyAborted` in the `core` crate, with rendering tests.
  - Files: crates/core/src/notification/events.rs,
    crates/core/tests/event_formatting_coverage.rs
  - Tests: test_tf_consistency_summary_message_variants,
    test_tf_consistency_aborted_message
- [x] `tf_consistency_audit` persistence module in the `storage` crate:
  DEDUP key `ts, trading_date_ist, feed, security_id, segment, tf,
  bucket_ts_ist, category, field`; ILP-over-HTTP writer with
  discard-pending poisoned-buffer defense; fail-soft ensure-DDL.
  - Files: crates/storage/src/tf_consistency_audit_persistence.rs,
    crates/storage/src/lib.rs
  - Tests: test_tf_consistency_audit_create_ddl_columns_and_dedup,
    test_tf_consistency_dedup_key_ts_first_segment_and_feed,
    test_finding_category_as_str_stable, test_append_finding_fills_buffer,
    test_tf_verify_flush_when_disconnected_errors_and_discards,
    test_tf_verify_discard_pending_clears_buffer_and_count,
    test_tf_verify_writer_uses_ilp_http_conf,
    test_ensure_tf_consistency_audit_table_mock_200 (+500/unreachable),
    test_tf_verify_writer_new_is_lazy
- [x] Verifier boot module in the `app` crate: pure grid/recompute/compare/
  classify/SQL/parse/decide fns + the async orchestrator + the
  process-global dual-spawn entry point.
  - Files: crates/app/src/tf_consistency_boot.rs, crates/app/src/lib.rs,
    crates/app/src/main.rs
  - Tests: test_bucket_grid_daily_counts_all_19_tfs,
    test_tripwire_grid_agrees_with_tf_index_bucket_start,
    test_recompute_window_first_max_min_last_sum,
    test_to_paise_boundaries, test_decide_tf_verify_start_variants,
    test_previous_trading_day_walks_back_over_weekend,
    test_select_1m_sql_micros_window_nanos_key_feed_filter_limit,
    test_select_tf_union_sql_has_19_arms_excludes_1m_and_1d,
    test_parse_1m_dataset_flags_rows_at_cap,
    test_classify_run_status_precedence (+ many more in the module)
- [x] Wiring-guard ratchet pinning both main.rs spawn sites + the
  no-skeleton stub-guard.
  - Files: crates/app/tests/tf_consistency_wiring_guard.rs
  - Tests: ratchet_tf_consistency_dual_spawn_sites,
    ratchet_tf_consistency_boot_module_is_not_a_stub
- [x] Runbook `.claude/rules/project/tf-consistency-error-codes.md`
  (forward crossref + runbook_path target + delivery-boundary note).
  - Files: .claude/rules/project/tf-consistency-error-codes.md
  - Tests: every_error_code_variant_appears_in_a_rule_file,
    every_runbook_path_exists_on_disk

## Design

At **15:40 IST** every trading day (const trigger, after the Dhan 15:30:05
close-time force-seal + writer drain and the 15:31 cross-verify burst,
before the 15:45 scoreboard), a cold-path task recomputes every stored
higher-timeframe candle (the 19 TFs `2m..4h` — `TfIndex::ALL` minus `M1`
the baseline minus `D1` which is excluded by design: Dhan drops D1 at the
write boundary and Groww D1 is a partial-day midnight bucket) from its
constituent `candles_1m` rows and compares exactly. Two passes per run:
`feed='dhan'` verifies TODAY (stable after the close seal); `feed='groww'`
verifies the PREVIOUS trading day (pure calendar walk-back ≤7 days — Groww
has no close-time force-seal, its tails seal at IST midnight, so D-1 gets
FULL coverage including final buckets and a missing D-1 tail row is a real
`missing_tf_row` page). The bucket grid is REIMPLEMENTED independently
(windows `[33_300 + k*S, min(+S, 55_800))` per trading day) and cross-pinned
against `TfIndex::bucket_start` by a hand-literal tripwire test so lockstep
drift is impossible. Query strategy per (feed, date): ONE discovery query
(DISTINCT security_id+segment over a UNION of the 20 candle tables, LIMIT
3000), then per SID Query A (`candles_1m` day rows, micros-window +
`(ts / 1) * 1000 AS ts_nanos` regression-locked shape, LIMIT 500) and Query
B (19-way UNION ALL across candles_2m..candles_4h tagged with a `tf` label,
LIMIT 2000) — sequential, one reqwest client, politeness yield every 25
SIDs, `returned_rows == LIMIT` is a truncation tripwire (read_degraded,
never a silent partial compare). Comparison: OHLC via integer paise
(`(x*100.0).round() as i64`, exact), volume exact i64 with checked_add
recompute (overflow → degraded), tick_count soft-counter only, oi + pct
columns never compared. Findings land in the new `tf_consistency_audit`
table (deterministic run ts = target day 15:40:00 IST → reruns UPSERT in
place; 10,000-row cap per run with a truncated flag), counters
`tv_tf_verify_*`, one coalesced TF-VERIFY-01 `error!` per (feed, date) pass
with ≥1 paging finding, and ONE `TfConsistencySummary` Telegram per run.
Spawned process-globally on BOTH boot paths (the scoreboard dual-spawn
pattern) with a once-per-process guard, self-gated on `[tf_consistency]
enabled` + trading day; env `TICKVAULT_TF_VERIFY_NOW` force-runs and
`TICKVAULT_TF_VERIFY_DATE=YYYY-MM-DD` backfills a past trading day.

## Edge Cases

- Sparse instruments (far-month futures): interior 1m gaps inside a window
  compare fine over present members; `bucket_gap` is a counter, never a
  finding row (would explode on legitimately sparse instruments).
- Stored TF row over ZERO member 1m rows → `no_1m_coverage` (pages).
- Recomputed present, stored row absent → `missing_tf_row` (pages) — the
  dead-seal-leg detector.
- Stored TF ts not on the 09:15-anchored grid → `off_grid_ts` (pages) —
  the purest anchoring-bug signal.
- Duplicate rows per DEDUP key in a response → `duplicate_key` (pages),
  compare proceeds on the FIRST row deterministically.
- Window boundary membership: `ts == w` in, `ts == w + S` out; final
  windows truncate at 15:30 (partial H1/H4 buckets compare correctly
  because no ≥15:30 1m row exists).
- Query returns rows == LIMIT → truncation tripwire → read_degraded.
- Weekend/holiday walk-back for the Groww D-1 date (≤7 days, else the
  Groww pass degrades loudly).
- Non-trading day: skip; `TICKVAULT_TF_VERIFY_NOW` without a DATE on a
  non-trading day is REFUSED with an info log (the scoreboard round-5
  mechanic); NOW + DATE backfills that past trading day.
- Late boot (post-15:40): RunCatchUp — runs immediately (idempotent).
- Volume i64 overflow on the recompute sum: checked_add → degraded, never
  a wrap.
- Feed off all day: zero rows both sides → `no_data` status, Info wording,
  never a daily High page and never a PASS claim.
- Always-on instruments (GIFT Nifty): excluded via the process-global
  always_on set (midnight-only seals + pre-open clamp semantics), counted.
- Mid-day-restart volume/day-open divergences: REPORTED as mismatches, the
  runbook explains recognition — never suppressed.

## Failure Modes

- QuestDB unreachable / HTTP client build failure → degraded summary
  (TF-VERIFY-02, stage client_build / questdb_unreachable / discovery),
  status BLIND, Telegram High — never a panic, never a blocked boot.
- Audit ILP flush refused → `error!` TF-VERIFY-02 stage=flush_failed +
  `discard_pending()` (poisoned-buffer defense) + degraded — the run can
  never read Pass over unpersisted findings.
- Ensure-DDL failure → fail-soft with the duplicate-row-window consequence
  NAMED in the error text (the HTTP-CLIENT-01 class).
- Run overruns the 900s budget → remaining SIDs read_degraded (stage
  budget_exceeded), status Degraded, summary still emitted before the
  16:30 auto-stop.
- Inner task panic/death → the outer supervisor pages
  `TfConsistencyAborted` (graceful-shutdown cancellation stays silent).
- A systemic bug flooding findings → the 10,000-row audit cap bounds the
  blast radius; counts stay exact; the summary carries `truncated`.

## Test Plan

Pure-fn unit tests dominate `crates/app/src/tf_consistency_boot.rs` (zero
`#[tokio::test]` in the boot file; the async orchestrator + spawn fn are
TEST-EXEMPT with the wiring guard cited): decide-fn variants + trigger pin,
previous-trading-day weekend walk-back, per-TF daily bucket counts pinned
as literals (2m=188 … 4h=2), the TfIndex literal TRIPWIRE (probe instants
09:15:00 … 15:29:59 + hand-typed literals 5m@09:17:30→09:15,
H1@15:20→15:15, M30@09:45→09:45), window boundary membership, recompute
fold (first/max/min/last/sum, gaps, single member, overflow), to_paise
boundaries (0.005 neighborhood, 0.0, negative, large), classify precedence
(blind vs no_data vs mismatch vs degraded vs pass), SQL builders (feed
filter + explicit LIMIT + micros window + nanos projection + 19 arms +
excludes candles_1m/candles_1d), parse fns (malformed skip, duplicate
detection, at-cap truncation flag), off_grid detection, audit-row cap,
budget degrade. Storage module near-fully tested including
`#[tokio::test]` mock `/exec` via `tokio::net::TcpListener 127.0.0.1:0`
canned 200/500 responses + the reserved-port-1 transport arm + lazy ILP
sender tests + the conf-string ratchet. Core rendering tests in
`event_formatting_coverage.rs`. Wiring guard source-scans both main.rs
spawn sites + the stub-guard. Common: config default-off trio + the
existing ErrorCode catalogue ratchets.

## Rollback

`[tf_consistency] enabled = false` (or deleting the section — serde default
OFF) → the spawn fn self-gates and returns before any task exists: zero
queries, zero rows, zero Telegram — byte-identical runtime. No hot-path
touch anywhere (cold once-daily task); no shared-table writes (reads
`candles_*`, writes ONLY `tf_consistency_audit` — live-feed purity clean).
Full revert = one PR removing the two modules + the arms; the audit table
is additive `CREATE IF NOT EXISTS` and stays inert.

## Observability

Counters (static labels, /metrics-local, no CloudWatch alarm, no
pre-registration): `tv_tf_verify_runs_total{status}`,
`tv_tf_verify_findings_total{category}`,
`tv_tf_verify_buckets_compared_total`,
`tv_tf_verify_query_failures_total{stage}`, `tv_tf_verify_bucket_gap_total`,
`tv_tf_verify_soft_divergence_total{field}`,
`tv_tf_verify_excluded_total{reason}`,
`tv_tf_verify_audit_rows_discarded_total`. Error codes TF-VERIFY-01
(coalesced, ≤10 named samples, never per-row) + TF-VERIFY-02 (per degraded
stage) — both log-sink-only; the operator page is the typed
`TfConsistencySummary` / `TfConsistencyAborted` Telegram (data-dependent
severity, BLIND-honest wording per audit Rule 11). Forensic record:
`tf_consistency_audit` QuestDB rows (feed-keyed DEDUP, deterministic run
ts). Runbook: `.claude/rules/project/tf-consistency-error-codes.md`.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy day, all TFs agree | status pass, Info Telegram, zero findings |
| 2 | A TF table's seal-writer leg died mid-day | mass `missing_tf_row` findings, High Telegram, TF-VERIFY-01 coalesced error |
| 3 | Aggregator anchoring bug (off-grid ts) | `off_grid_ts` findings, High page, runbook halt-trust escalation |
| 4 | QuestDB down at 15:40 | BLIND status, High Telegram, TF-VERIFY-02, next day retries |
| 5 | Groww disabled all day | Groww pass no_data (Info wording), Dhan pass verified normally |
| 6 | Weekend force-run without DATE | refused with info log, no page |
| 7 | Backfill `TICKVAULT_TF_VERIFY_DATE` of a past trading day | reruns UPSERT in place (deterministic 15:40 ts) |
| 8 | 1200-SID day | bounded per-SID queries, ≤900s budget, Degraded on overrun |
