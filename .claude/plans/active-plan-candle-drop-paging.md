# Implementation Plan: AGGREGATOR-DROP-01 pages nobody — CloudWatch alarm + seal-drop counter export

**Status:** VERIFIED
**Date:** 2026-07-09
**Approved by:** Parthiban (operator) — PR #2 of the 2026-07-09 5-PR serial sequence (audit-confirmed gap: the ONLY silent-data-loss path for sealed candles is Severity::Critical yet reaches no pager)
**Crates:** `app` (`crates/app/src/main.rs` — one counter pre-registration, cold boot path) + terraform/docs/guard-tests

Per-item 15-row + 7-row guarantee matrices: cross-reference
`.claude/rules/project/per-wave-guarantee-matrix.md` (applied below, item-scoped).

## Design

The 2026-07-09 audit confirmed `AGGREGATOR-DROP-01`
(`ErrorCode::AggregatorDrop01`, Severity::Critical — a sealed candle dropped
after ring+spill+DLQ all failed) currently pages NOBODY:

1. No entry in the `error_code_alerts` map
   (`deploy/aws/terraform/error-code-alarms.tf`) — the canonical
   `error!` → errors.jsonl → CW Logs → filter → `tv_errcode_*` → alarm →
   SNS → Telegram chain covers only the 8 codes listed in
   `observability-architecture.md` "Which codes page (2026-07-06)".
2. The companion counter `tv_seal_writer_drain_total{kind="dropped"}`
   (`crates/storage/src/seal_writer_loop.rs::record_cycle_observability`)
   is NOT exported to CloudWatch — it is not in the EMF metric_selectors
   allowlist and has no /metrics log-filter fallback, so no metric-side
   alarm exists either.

Two paging routes, both mirroring existing house patterns exactly:

- **Route (a) — errcode log-filter alarm:** one new `error_code_alerts` map
  entry `"aggregator-drop-01"` in `error-code-alarms.tf` (pattern
  `{ $.code = "AGGREGATOR-DROP-01" && $.level = "ERROR" }`, period 300,
  threshold 1, eval 3 / dta 1, `ok_recovery = false` — a drop is a discrete
  data-loss event; auto-OK ~15 min later would be a Rule-11 false recovery,
  the PROC-01 precedent). The emit site is
  `crates/storage/src/seal_writer_loop.rs:178`
  (`error!(code = ErrorCode::AggregatorDrop01.code_str(), dropped = …)`) —
  ERROR level, so it reaches the errors.jsonl stream the filter watches.
- **Route (b) — counter export + alarm:** the exact
  `feed-stall-restart-alarm.tf` house pattern — a new
  `deploy/aws/terraform/seal-drop-alarm.tf` with a log metric filter on the
  agent-created `/tickvault/<env>/metrics` group (the counter is NOT in the
  ratcheted EMF allowlist; every 60s scrape still ships it as a plain-JSON
  event with `$.host` and the `kind` label as a field), pattern
  `{ $.tv_seal_writer_drain_total = * && $.kind = "dropped" }`, extracting
  the per-scrape delta into a DISTINCT derived metric name
  `tv_seal_writer_drain_dropped_total` (a kind-restricted slice must not
  reuse the raw counter's identity — a future unfiltered extraction would
  double-count under Sum), dimension `[host]`, alarmed at Sum ≥ 1 per
  aligned 300s window, `treat_missing_data = notBreaching`, always armed
  (a sealed-candle drop is never routine, including the IST-midnight
  force-seal burst — no market-hours gate).
- **Pre-registration (the round-5 feed-stall lesson):** the CW agent's
  delta pipeline DROPS each counter series' first observed sample as its
  baseline. `tv_seal_writer_drain_total{kind="dropped"}` increments ONLY
  when seals are truly dropped, so the series would be BORN at the first
  drop (value N) and the dropped baseline sample would BE the drop —
  a single-episode drop (the dominant shape) would produce ZERO datapoints
  and route (b) would be dead on arrival. Fix: pre-register the
  `kind="dropped"` series at 0 in `crates/app/src/main.rs` immediately
  AFTER `observability::init_metrics` (next to the existing
  `tv_feed_sidecar_stall_restart_total` pre-registration), making the
  series dense from boot so the dropped first sample is the harmless 0.
- **Docs sync:** `observability-architecture.md` "Which codes page" list +
  a dated 2026-07-09 update note under AGGREGATOR-DROP-01 in
  `wave-6-error-codes.md` (append-only, house style).
- **Housekeeping:** archive the merged
  `active-plan-retention-sweep-fix.md` →
  `.claude/plans/archive/2026-07-09-retention-sweep-fix.md`.

## Edge Cases

- **Single-cycle drop episode (dominant shape):** route (a) pages on the one
  ERROR line; route (b) pages on the one non-zero delta — but ONLY because
  of the pre-registration (without it the delta pipeline eats the first
  sample; see Design).
- **Repeating drops (host catastrophically broken):** the emit fires per
  drain cycle; eval-3/dta-1 holds ALARM across ≤15-min repeat gaps without
  flapping (the DH-901 shape rationale in the locals header).
- **IST-midnight force-seal burst drop:** outside market hours — both
  alarms are deliberately always-armed (data loss is never routine).
- **App restart:** counter resets; the agent's delta calculation absorbs it
  (first post-restart sample re-dropped — harmless 0 because the
  pre-registration re-runs every boot).
- **Other `kind` label values (submitted/flushed_rows/…):** excluded by the
  `$.kind = "dropped"` pattern clause; they never inflate the Sum.
- **Pre-install drop window:** a seal drop between seal-writer spawn and the
  Step-2 recorder install would increment a no-op handle — physically
  implausible (the seal writer only drops after ring+spill+DLQ all fail,
  and boot has no seal traffic in that prefix); same stated residual class
  as the stall-counter registration.

## Failure Modes

- **Filter pattern never matches** (the 2026-07-06 zero-page class): guarded
  by mirroring the 8 existing entries byte-for-byte in shape, and by the new
  wiring guard test pinning the tf pattern AND the Rust emit site's
  `code_str()` literal in lockstep.
- **Metric-name drift** (Rust rename vs tf): the wiring guard test pins
  `tv_seal_writer_drain_total` + `kind => "dropped"` in
  `seal_writer_loop.rs`, the main.rs pre-registration, and both tf files —
  any rename fails the build.
- **Counter proves CUMULATIVE not delta-shipped:** same stated residual as
  feed-stall/order-update — Sum would over-page (fail-loud, never a silent
  miss); rework to DIFF(Maximum) if the post-apply verification shows it.
- **EMF allowlisting later:** if `tv_seal_writer_drain_total` is ever added
  to the EMF metric_selectors allowlist, the log metric filter must be
  removed in the same PR (double-count under Sum halves the threshold —
  documented in the tf header, feed-stall precedent).
- **Pre-registration placed before the recorder installs:** resolves to the
  no-op recorder and registers nothing (the VOID round-4 class) — the guard
  test is a source-order scan (install index < registration index).

## Test Plan

- NEW `crates/app/tests/seal_drop_paging_wiring_guard.rs` (4 tests):
  1. `test_seal_drop_counter_is_preregistered_after_recorder_install` —
     source-order scan of main.rs: `observability::init_metrics(` index <
     `"tv_seal_writer_drain_total"` index, and the registration window names
     `"dropped"`.
  2. `test_seal_writer_loop_still_emits_coded_error_and_dropped_counter` —
     stub-guard on `crates/storage/src/seal_writer_loop.rs`: the production
     region still contains `ErrorCode::AggregatorDrop01.code_str()` inside
     an `error!` and the `kind => "dropped"` counter increment.
  3. `test_errcode_alarm_map_contains_aggregator_drop_01` — the tf map entry
     exists with the exact JSON-filter pattern matching the emitted code
     string + ERROR level, and `ok_recovery = false`.
  4. `test_seal_drop_fallback_filter_matches_emitted_series_shape` — the new
     tf file's filter pattern names the real metric field + the `dropped`
     kind, extracts `$.tv_seal_writer_drain_total` into the derived
     `tv_seal_writer_drain_dropped_total` name, and the alarm reads that
     same name with Sum/300s/threshold 1/notBreaching.
- Run: `cargo test -p tickvault-app --test seal_drop_paging_wiring_guard`
  + the neighbouring pre-registration ratchet
  (`groww_sidecar_supervisor` tests) to prove no regression
  + `cargo clippy -p tickvault-app -- -D warnings`.
- `bash .claude/hooks/banned-pattern-scanner.sh` +
  `bash .claude/hooks/plan-verify.sh`.
- terraform validate: NOT installed in this sandbox — the Deploy Lint CI
  job validates (stated honestly, not faked).

## Rollback

- Terraform: delete the `"aggregator-drop-01"` map entry + the
  `seal-drop-alarm.tf` file; `terraform apply` destroys the filter + alarms
  cleanly (no state migration — for_each keyed resources).
- Rust: remove the one `.increment(0)` pre-registration block in main.rs +
  the guard test file. No behavioural change to the seal writer itself —
  this PR touches ZERO hot-path code (the pre-registration is one cold
  boot-time call).
- Docs: the dated 2026-07-09 notes are append-only; revert removes them
  without history rewrite.

## Observability

- Route (a): `tv_errcode_aggregator_drop_01` metric + alarm
  `tv-<env>-errcode-aggregator-drop-01` → SNS `tv_alerts` → Telegram,
  ≤5 min from the ERROR line.
- Route (b): `tv_seal_writer_drain_dropped_total` [host] metric + alarm
  `tv-<env>-seal-writer-dropped` → same SNS route.
- The existing `error!` line, errors.jsonl forensic sink, triage rule
  (`error-rules.yaml::aggregator-drop-01-sealed-candle-lost-escalate`),
  runbook (`wave-6-error-codes.md`) and the per-minute
  `seal writer progress` info! report are all unchanged.
- Cost delta: +2 alarms (~$0.20/mo) + up to 2 sparse/dense custom metric
  series (~$0.60/mo worst case) ≈ **$0.80/mo pre-GST (~₹80/mo incl. GST)**
  — CloudWatch only (an EXISTING service; no new paid AWS service), inside
  the `aws-budget.md` $35/mo pre-GST budget-alarm ceiling.

## Plan Items

- [x] Archive merged retention-sweep plan to
      `.claude/plans/archive/2026-07-09-retention-sweep-fix.md`
  - Files: .claude/plans/archive/2026-07-09-retention-sweep-fix.md
- [x] Route (a): `"aggregator-drop-01"` entry in `error_code_alerts`
  - Files: deploy/aws/terraform/error-code-alarms.tf
  - Tests: test_errcode_alarm_map_contains_aggregator_drop_01
- [x] Route (b): seal-drop counter export + alarm (feed-stall pattern)
  - Files: deploy/aws/terraform/seal-drop-alarm.tf
  - Tests: test_seal_drop_fallback_filter_matches_emitted_series_shape
- [x] Pre-register `tv_seal_writer_drain_total{kind="dropped"}` at 0
      post-recorder-install
  - Files: crates/app/src/main.rs
  - Tests: test_seal_drop_counter_is_preregistered_after_recorder_install,
    test_seal_writer_loop_still_emits_coded_error_and_dropped_counter
- [x] Docs sync (dated 2026-07-09, append-only)
  - Files: .claude/rules/project/observability-architecture.md,
    .claude/rules/project/wave-6-error-codes.md

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Single drain cycle drops N seals (ring+spill+DLQ all failed) | ONE ERROR line → errcode alarm pages ≤5 min; ONE non-zero counter delta → seal-writer-dropped alarm pages (pre-registration makes the delta visible) |
| 2 | Healthy session, zero drops | Dense 0-delta events; both alarms stay OK; no page |
| 3 | IST-midnight force-seal burst drop | Both alarms page (always-armed) |
| 4 | App restart mid-session | Counter re-registered at 0 each boot; no false page, no eaten first drop |
| 5 | Rust metric/code-string rename | Wiring guard test fails the build |
