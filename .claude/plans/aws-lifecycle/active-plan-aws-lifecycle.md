# Active Plan — AWS Daily Lifecycle Hardening

**Status:** DRAFT (design discussion only, no PRs opened)
**Date:** 2026-05-18
**Approved by:** pending operator lock
**Companion doc:** `docs/architecture/aws-daily-lifecycle.md`

## Scope

Close the 12 gaps in §9 of the design doc — wire the OUT-OF-BAND deadman notification path so the 12 extreme worst-case scenarios in §3 all reach the operator within 60s.

## Locks captured 2026-05-18

- [x] **LOCK-1** — 08:00 IST start, 17:00 IST stop weekdays; 08:00/13:00 weekends. Terraform wins; aws-budget.md to be aligned in Item AWS-1.
- [ ] **LOCK-2** — Operator picked "<₹1K, accept compromises TBD." Honest analysis in `docs/architecture/aws-cost-floor-analysis.md` shows sub-₹1K NOT achievable on AWS without breaking invariants. **PENDING: operator picks A/B/C/D from §6 of that doc.**
- [x] **LOCK-3** — Out-of-band Telegram via SNS → Lambda → Telegram Bot API (Item AWS-7).
- [ ] **LOCK-4** — DLT registration status for SNS-SMS India sender ID — operator to confirm.
- [x] **LOCK-5** — Stop at SNS 3-leg fan-out. No Amazon Connect phone-call escalation. Item AWS-10 removed from mandatory list; kept as optional. Kill switch auto-engagement at 09:14:30 IST is the backstop.
- [ ] **LOCK-6** — Multi-AZ fallback (Item AWS-11) — pending operator decision (accept envelope or implement).
- [ ] **LOCK-7** — Cross-region SNS (Item AWS-12) — pending operator decision.

## Plan items (each carries Z+ 7-layer + 15-row + 7-row matrices per operator-charter §C)

### Item AWS-1 — Lock the schedule

- Files: `deploy/aws/terraform/main.tf:256,262,268,274`, `.claude/rules/project/aws-budget.md` rule 8
- Tests: ratchet test that asserts the schedule string matches across all 3 locations
- Effort: 1 PR, ~5 LoC

### Item AWS-2 — EventBridge RetryPolicy + DLQ

- Files: `deploy/aws/terraform/main.tf:253-275`, new `deploy/aws/terraform/dlq.tf`
- Adds: `aws_sqs_queue.tv_eventbridge_dlq`, `retry_policy { maximum_retry_attempts = 3, maximum_event_age = 900 }` on each of the 4 rules
- Tests: terraform plan diff + a CW alarm "DLQ depth > 0" with SNS routing
- Catches: cases A, B
- Effort: 1 PR, ~60 LoC

### Item AWS-3 — "Missing invocation" alarm per rule

- Files: `deploy/aws/terraform/alarms.tf`
- Adds 4 CW alarms keyed on `AWS/Events Invocations` metric per rule name, with `TreatMissingData=breaching`, evaluation window 10min
- Tests: terraform plan diff; manual trigger via `aws events disable-rule` in dev account
- Catches: case A (rule disabled/deleted)
- Effort: 1 PR, ~80 LoC

### Item AWS-4 — `tv_boot_complete` gauge + alarm

- Files: `crates/app/src/main.rs` (emit `metrics::gauge!("tv_boot_complete", 1.0)` after Step 15), `deploy/aws/terraform/alarms.tf` (alarm)
- Adds: app-side gauge; CW alarm "metric missing by T+240s from instance state-change-to-running"
- Tests: integration test that asserts the gauge is set; chaos test that kills the app mid-boot and confirms alarm fires
- Catches: cases D, E, F, J
- Effort: 1 PR, ~40 LoC Rust + ~40 LoC Terraform

### Item AWS-5 — `tv_app_alive_metric` heartbeat gauge

- Files: `crates/app/src/observability.rs` (background tokio task setting gauge every 30s during market hours)
- Adds: CW alarm `TreatMissingData=breaching`, 60s window
- Tests: chaos test (kill app post-boot, confirm alarm fires within 60s)
- Catches: case K (mid-day crash)
- Effort: 1 PR, ~50 LoC

### Item AWS-6 — CW Logs subscription filter on systemd exit nonzero

- Files: `deploy/aws/terraform/alarms.tf` (subscription filter + dest)
- Adds: filter pattern matching `tickvault.service: Main process exited, code=exited, status=<non-zero>`; routes to SNS via Lambda or direct
- Tests: terraform plan; manual injection via systemd-run failing command
- Catches: case F (binary exit loop)
- Effort: 1 PR, ~50 LoC

### Item AWS-7 — SNS fan-out wiring (SMS + Email + Telegram-webhook)

- Files: new `deploy/aws/terraform/sns-subscriptions.tf`, new `deploy/aws/lambda/sns-to-telegram/` (small Lambda)
- Adds: 3 SNS subscriptions on `tv-prod-alerts` topic. Lambda converts SNS message → Telegram bot.sendMessage call (token via SSM).
- Tests: terraform plan; end-to-end test in dev (publish to topic, verify 3 receipts)
- Catches: out-of-band delivery for ALL cases that route via SNS
- Effort: 1 PR, ~150 LoC

### Item AWS-8 — Operator daily startup integration

- Files: `docs/runbooks/operator-daily-startup.md` (extend §A with the out-of-band alarm names)
- Adds: documentation of which SNS topic receives which alarm
- Effort: 1 PR, ~30 LoC docs

### Item AWS-9 — Pre-baked AMI with CW Logs agent (OPTIONAL)

- Files: new `deploy/aws/packer/`
- Adds: Packer config that builds AMI with CW Logs agent pre-installed + tailing `/var/log/cloud-init-output.log`
- Catches: case D visibility upgrade (NOT necessary if Item AWS-4 boot-complete alarm is in place; provides faster diagnostic)
- Effort: 1 PR, ~200 LoC

### Item AWS-10 — Escalation Lambda for operator AWOL (OPTIONAL)

- Files: new `deploy/aws/lambda/escalation/`
- Adds: Lambda triggered 10 min after primary SNS publish; checks an "acknowledged" SSM parameter; if false → fires secondary SNS topic / phone CALL
- Catches: case L (operator AWOL)
- Effort: 1 PR, ~150 LoC

### Item AWS-11 — Multi-AZ fallback (OPTIONAL, accepts envelope today)

- Files: new SSM Automation document `tv-start-with-multi-az-fallback`
- Adds: if primary AZ returns InsufficientInstanceCapacity, retry in alternate AZ
- Catches: case C
- Effort: 1 PR, ~120 LoC

### Item AWS-12 — Cross-region SNS for ap-south-1 outage envelope (OPTIONAL)

- Files: new `deploy/aws/terraform/sns-cross-region.tf`
- Adds: secondary SNS topic in `us-east-1`; CW alarms publish to BOTH topics (or to a single SNS Cross-Region Replication target)
- Catches: ap-south-1 region SNS outage
- Effort: 1 PR, ~80 LoC

## Sequencing (operator-charter §H — one PR at a time)

```
LOCK-1 ──► AWS-1 ──► AWS-2 ──► AWS-3 ──► AWS-4 ──► AWS-5 ──► AWS-6 ──► AWS-7 ──► AWS-8
                                                                                    │
                                                                          (optional from here)
                                                                                    │
                                                                  AWS-9 / AWS-10 / AWS-11 / AWS-12
```

Total: 7 mandatory PRs + 4 optional. Estimated calendar time: 2-3 weeks at 1 PR per session.

## Scenarios coverage matrix

| Worst-case | Caught by item |
|---|---|
| A — Rule disabled | AWS-3 |
| B — IAM/throttle | AWS-2 |
| C — Capacity | AWS-11 (optional; otherwise manual) |
| D — cloud-init | AWS-4 |
| E — docker dead | AWS-4 |
| F — binary fail | AWS-4 + AWS-6 |
| G — Dhan auth | (already covered: in-band Telegram via runbook `auth.md`) |
| H — QuestDB | (already covered: BOOT-01/02 in-band + AWS-4 backup) |
| I — WS connect | (already covered: existing runbooks) |
| J — OOM boot | AWS-4 |
| K — Crash mid-day | AWS-5 |
| L — Operator AWOL | AWS-10 (optional; otherwise accept envelope) |
