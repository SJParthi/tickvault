# Implementation Plan: EC2 start/stop schedule resilience — EventBridge retry + DLQ + DLQ alarm

**Status:** APPROVED
**Date:** 2026-06-15
**Approved by:** Parthiban (2026-06-15, verbatim: "yes dude our ultimate aim is to make it extremely comprehensively automated bro without any human inputs needed")
**Branch:** `claude/upbeat-hypatia-6trzk3`
**Authority:** CLAUDE.md > operator-charter-forever.md > aws-budget.md > design-first-wall.md > this file
**Scope guard:** `deploy/aws/terraform/` + `.claude/plans/` only — NO `crates/*/src`. Plan-gate exempt. Applies to live infra via `terraform-apply.yml` on merge.

## Design

The 08:30 IST EC2 auto-start (`daily_start` EventBridge rule → SSM `AWS-StartEC2Instance`)
has failed repeatedly (Jun 08/11/15), forcing the 08:45 watchdog + the operator to start
the box manually. Root cause (verified in `main.tf`): the `daily_start`/`daily_stop`
EventBridge targets have **no `retry_policy` and no `dead_letter_config`** — a single
transient SSM/EC2 throttle silently drops the start with zero retries and nothing in a DLQ
to inspect. The IAM role is correct (not a permissions bug). The intermittent pattern fits
transient-failure-with-no-retry. The `aws-startinstances-failed.md` runbook even references
a DLQ (`tv-prod-eventbridge-dlq`) + a "DLQ depth alarm" that **do not exist in terraform**.

Fix: make the primary path self-healing so the watchdog/human are the exception, not the
norm:
1. Create the SQS DLQ the runbook already assumes + a queue policy letting EventBridge send to it.
2. Add `retry_policy` (5 attempts / 600s window) + `dead_letter_config` to `daily_start` AND `daily_stop` targets — a transient hiccup now retries; a persistent failure lands in the DLQ instead of vanishing.
3. Add a CloudWatch alarm on DLQ depth > 0 → SNS, so an *exhausted-retries* (persistent) failure auto-pages with zero polling — closing the "no human input" loop the runbook promised.

Honest envelope: this fixes the recurring **transient start-drop**. It does NOT fix the
separate Jun 08/11 **service** fail-loop (Case F — app boot error), which needs the box's
`journalctl` (not visible from the repo). And the apply runs on AWS via `terraform-apply.yml`
— I cannot `terraform plan/apply` from the dev sandbox, so the HCL is written to the codebase
convention but not plan-validated here.

## Plan Items

- [x] Item 1 — New `deploy/aws/terraform/eventbridge-dlq.tf`: `aws_sqs_queue` (14-day retention) + `aws_sqs_queue_policy` (allow `events.amazonaws.com` SendMessage, scoped to the daily_start/daily_stop rule ARNs).
  - Files: `deploy/aws/terraform/eventbridge-dlq.tf`
- [x] Item 2 — New CloudWatch alarm in the same file: `ApproximateNumberOfMessagesVisible > 0` on the DLQ → `aws_sns_topic.tv_alerts` (auto-page on exhausted-retry start failure).
  - Files: `deploy/aws/terraform/eventbridge-dlq.tf`
- [x] Item 3 — Add `retry_policy { maximum_retry_attempts = 5, maximum_event_age_in_seconds = 600 }` + `dead_letter_config { arn = ... }` to the `daily_start` AND `daily_stop` targets in `main.tf`.
  - Files: `deploy/aws/terraform/main.tf`

## Edge Cases
- DLQ retention 14 days so a Friday-evening failure is still inspectable Monday.
- Queue policy scoped by `aws:SourceArn` to ONLY the two schedule rule ARNs (least privilege).
- Alarm `treat_missing_data = notBreaching` — an empty DLQ emits no datapoints; must not false-fire (same lesson as the EC2 status-check alarms).
- `retry_policy` + `dead_letter_config` are in-place target updates (no resource recreate) — safe apply.

## Failure Modes
- If the DLQ alarm itself can't reach SNS → same blast radius as every other alarm (SNS topic is shared, already wired). No new single point of failure.
- If retries still exhaust (real outage) → event lands in DLQ → alarm pages → 08:45 watchdog still independently starts the box. Defense in depth preserved.

## Test Plan
- No terraform binary in the sandbox → cannot `terraform validate`/`plan` here (honest). HCL written to the exact codebase convention (matches `alarms.tf` alarm shape + `main.tf` target shape + provider `~> 5.80`).
- `terraform-apply.yml` runs `fmt -check` + `validate` + `plan` + `apply` on merge — that is the real gate.
- Pre-push gates: banned-pattern, plan-verify, secret-scan (no secrets added).

## Rollback
- Additive infra (new SQS + alarm) + two in-place target updates. `git revert <sha>` + re-apply removes the retry/DLQ and deletes the queue/alarm — the schedule reverts to today's behaviour. No data, no instance impact.

## Observability
- This IS an observability/automation fix: a previously-silent start-drop now (a) retries, (b) lands in an inspectable DLQ, (c) auto-pages via CloudWatch→SNS. Makes the `aws-startinstances-failed.md` runbook's referenced DLQ + alarm actually exist.

## Guarantee matrix

The full 15-row + 7-row matrices from `per-wave-guarantee-matrix.md` apply. Applicable rows:
100% alerting (new DLQ-depth alarm → SNS), 100% monitoring (DLQ depth metric), 100%
scenarios (transient + persistent start-failure both covered), 100% recovery (retry → DLQ →
watchdog defense-in-depth). N/A rows — code coverage/DHAT/bench/hot-path O(1) — reason:
terraform-only change, zero `crates/*/src`. Any "100% guarantee" here is bounded "100% inside
the tested envelope, with ratcheted regression coverage" per `wave-4-shared-preamble.md` §8 —
and explicitly does NOT cover the separate Case-F service fail-loop.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | Transient SSM/EC2 throttle at 08:30 | EventBridge retries up to 5× / 600s → start succeeds; no human input |
| 2 | Persistent start failure (real outage) | Event lands in DLQ → CloudWatch alarm → SNS page; 08:45 watchdog independently starts box |
| 3 | Normal trading day | Start succeeds first try; DLQ empty; alarm stays OK |
| 4 | Operator reads aws-startinstances-failed.md | The referenced `tv-<env>-eventbridge-dlq` + depth alarm now actually exist |
