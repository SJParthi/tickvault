# Implementation Plan: B5 — AWS alarm semantics + disk (EBS latency metric-math, EventBridge DLQ edge-alarm)

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — B5 task directive 2026-07-03 ("ebs-write-latency → metric-math per-op latency; eventbridge-dlq → age-based or drain-after-inspect; verify boot-heartbeat; verify S3 archival")

Per-item guarantee matrix: cross-reference .claude/rules/project/per-wave-guarantee-matrix.md (15-row + 7-row matrices apply to every item in this plan).

## Plan Items

- [x] (a) Fix `ebs_write_latency` alarm: VolumeTotalWriteTime is cumulative seconds per period, not ms latency — convert to metric-math per-op latency `IF(ops > 0, (wt / ops) * 1000, 0)` (Sum VolumeTotalWriteTime / Sum VolumeWriteOps × 1000), threshold 50 ms/op, same actions/evaluation.
  - Files: deploy/aws/terraform/alarms.tf
  - Tests: test_ebs_write_latency_alarm_uses_metric_math_per_op_latency

- [x] (b) Fix EventBridge DLQ alarm latch: ApproximateNumberOfMessagesVisible Maximum > 0 latches ALARM for the whole SQS retention — switch to NumberOfMessagesSent Sum > 0 / 300s (edge-trigger on NEW dead-letters, self-clears), and bound the queue retention to 259200s (3-day drain-after-inspect). Resource + alarm names kept stable to avoid delete/recreate churn.
  - Files: deploy/aws/terraform/eventbridge-dlq.tf
  - Tests: test_eventbridge_dlq_alarm_is_edge_triggered_on_new_arrivals, test_eventbridge_dlq_retention_is_bounded_drain_after_inspect

- [x] (c) VERIFY-ONLY: boot-heartbeat chain coherence (emitter main.rs `tv_boot_completed` gauge 1.0 → CWAgent EMF selector source_labels ["host"] / ^tickvault-prod$ → log-metric-filter fallback Tickvault/Prod + host dim → alarm namespace/dimensions match). No code change; pinned by ratchet.
  - Files: (none changed) crates/app/src/main.rs, deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/metrics-log-metric-filters.tf, deploy/aws/terraform/boot-heartbeat-alarm.tf
  - Tests: test_boot_heartbeat_alarm_contract_unchanged

- [x] (d) VERIFY-ONLY: >90d S3 archival status — partition_manager.rs honestly documents DETACH-only (local `detached/` dir; S3 upload NOT implemented), wired inline post-market in main.rs, retention_days default 90. No src change (report only).
  - Files: (none changed) crates/storage/src/partition_manager.rs, crates/app/src/main.rs, crates/common/src/config.rs
  - Tests: n/a (verification finding, reported to coordinator)

- [x] Ratchet test file pinning (a)+(b)+(c) contracts.
  - Files: crates/common/tests/aws_alarm_semantics_guard.rs
  - Tests: test_ebs_write_latency_alarm_uses_metric_math_per_op_latency, test_eventbridge_dlq_alarm_is_edge_triggered_on_new_arrivals, test_eventbridge_dlq_retention_is_bounded_drain_after_inspect, test_boot_heartbeat_alarm_contract_unchanged

## Design

Two CloudWatch alarms carried semantics bugs that made them either unable to fire meaningfully or unable to clear:

1. **EBS write latency** (`deploy/aws/terraform/alarms.tf`): `VolumeTotalWriteTime` reports the cumulative seconds of write time in the period. Averaging it and comparing to a "50 ms" threshold mixes units (seconds-per-period vs ms-per-op). The fix converts the alarm to CloudWatch metric math with three `metric_query` blocks: `wt` = Sum(VolumeTotalWriteTime), `ops` = Sum(VolumeWriteOps), `e1` = `IF(ops > 0, (wt / ops) * 1000, 0)` — genuine average milliseconds per write operation, guarded against divide-by-zero on idle periods. Threshold stays 50 (now truly ms/op), 3 × 300s evaluation, `notBreaching`, same SNS actions. Terraform forbids mixing `metric_query` with top-level `metric_name`/`namespace`/`statistic`/`period`/`dimensions`, so those move inside the query blocks.

2. **EventBridge DLQ** (`deploy/aws/terraform/eventbridge-dlq.tf`): `ApproximateNumberOfMessagesVisible Maximum > 0` is a level signal — one dead-lettered message keeps the alarm red for the entire (previously 14-day) retention even after the operator inspects it. The fix is edge-trigger + drain-after-inspect: alarm on `NumberOfMessagesSent Sum > 0` per 300s (fires only the period a NEW message arrives, self-clears next period) and bound retention to 3 days (259200s) so payloads stay inspectable across a weekend then self-drain. Resource and alarm names are deliberately kept (`eventbridge_dlq_depth` / `...-dlq-depth`) to avoid a delete/recreate of the live alarm.

Items (c) and (d) are verification-only per the task directive; (c) additionally gains a ratchet pin so the coherent chain cannot silently drift.

## Edge Cases

- **Idle volume (ops = 0):** the `IF(ops > 0, ..., 0)` expression returns 0 instead of a divide-by-zero NaN/missing point, so an idle disk can never false-page or produce INSUFFICIENT_DATA flapping.
- **Missing EBS datapoints (instance stopped 16:30–08:30 IST):** `treat_missing_data = "notBreaching"` on both alarms — the nightly stop never pages.
- **Two dead-letters in the same 300s period:** Sum = 2 > 0 — one page, correct (one decision per alarm).
- **Dead-letter arrives, operator inspects, message ages out at 3 days:** alarm already returned to OK after the arrival period; retention expiry causes no state change.
- **Alarm rename churn:** avoided — names kept stable, only metric/description/retention change in-place.

## Failure Modes

- **Terraform apply mixes query + top-level metric fields:** would fail `terraform validate`/apply; the edit removed all top-level metric fields from the ebs alarm (ratchet pins `metric_query` presence and bans the old `statistic = "Average"` shape inside the block).
- **Future regression back to depth-latch or raw-seconds metric:** `crates/common/tests/aws_alarm_semantics_guard.rs` fails the build (source-scan ratchet, same mechanism as `cloudwatch_app_alarms_wiring.rs`).
- **NumberOfMessagesSent not emitted:** SQS emits it on every SendMessage from EventBridge's DLQ delivery; with zero sends the metric is missing → `notBreaching` → OK state, which is the honest quiet state.
- **Retention shortened below inspect window:** ratchet allows ≤ 259200 only; the comment in the .tf documents why 3 days.

## Test Plan

- `cargo test -p tickvault-common --test aws_alarm_semantics_guard` — 4 source-scan ratchet tests (metric-math pin, edge-trigger pin, retention bound, boot-heartbeat contract).
- `terraform fmt` on the two changed .tf files; `terraform validate` attempted if terraform + network available (noted honestly if not).
- Existing guards unaffected: no `metric_name = "tv_*"` lines added/removed in app-alarms.tf, no EMF list change, no src change (plan-gate untouched).

## Rollback

`git revert` of this PR's commit restores the previous single-metric EBS alarm, the depth-latch DLQ alarm, and 14-day retention; the ratchet test file is removed by the same revert so no guard blocks the rollback. Terraform apply of the reverted state performs in-place alarm updates + queue attribute update (no destroy of the queue — retention is an attribute).

## Observability

- Both alarms keep their SNS `alarm_actions` to `aws_sns_topic.tv_alerts` (Telegram/Email fan-out); the DLQ alarm keeps `ok_actions` so recovery is visible.
- Alarm descriptions now state the exact semantics (metric-math formula; edge-trigger + 3-day drain window) so the operator reading the page knows what fired and why it will clear.
- The (c) boot-heartbeat chain (emitter → EMF selector → log-metric-filter fallback → alarm) is now pinned by `test_boot_heartbeat_alarm_contract_unchanged` in addition to the pre-existing `cloudwatch_app_alarms_wiring.rs` guards.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | EBS avg per-op write latency 60ms for 15 min | e1 > 50 for 3 periods → ALARM → SNS page |
| 2 | Idle volume overnight (instance stopped) | missing data → notBreaching → OK, no page |
| 3 | EventBridge start invocation dead-letters at 08:30 IST | NumberOfMessagesSent Sum=1 in that 300s period → ALARM + page → self-clears next period |
| 4 | Stale inspected DLQ message sits 2 days | no new sends → alarm stays OK; message self-drains at 3 days |
| 5 | Someone reverts DLQ alarm to ApproximateNumberOfMessagesVisible | `test_eventbridge_dlq_alarm_is_edge_triggered_on_new_arrivals` fails the build |
