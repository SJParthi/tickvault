# Implementation Plan: Fix AWS auto-start miss + deploy-on-stopped-box spam

**Status:** VERIFIED
**Date:** 2026-06-02
**Approved by:** Parthiban (2026-06-02 — "Fixes B + C + diagnostic", normal trading day)

> Per-wave / per-item guarantee matrix: cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row) and
> `.claude/rules/project/zero-loss-guarantee-charter.md`. This is a CI/infra
> change (no Rust hot-path, no QuestDB schema, no tick path) — the resilience
> rows are N/A; the proof rows are the ratchet tests listed below.

## Incident

2026-06-02 08:45 IST: EC2 box still stopped (08:30 IST EventBridge start did not
succeed) → every `deploy-aws` run failed at `aws ssm send-command`
(`InvalidInstanceId: Instances not in a valid state`) → 5× "DLT deploy FAILED"
Telegram overnight. `aws-autopilot` only self-started during 09:00-15:35 IST, so
the 08:30-09:00 gap was uncovered.

## Plan Items

- [x] **B — deploy-aws: do not page on an intentional off-hours skip**
  - Files: .github/workflows/deploy-aws.yml
  - Behaviour: when a stopped box is auto-pushed-to outside the 08:30-16:30 IST
    up-window, set `DEPLOY_SKIP_REASON=intentional_stopped`, gate the
    failure-notify + auto-stop + fetch-output steps on it (no Telegram); run
    stays non-success so the 08:45 cron redeploys.
  - Tests: test_deploy_aws_self_starts_stopped_instance_and_skips_offhours_cleanly

- [x] **C — deploy-aws: self-start a stopped box that SHOULD be up**
  - Files: .github/workflows/deploy-aws.yml
  - Behaviour: "Ensure instance running" step starts the box + waits for SSM
    `PingStatus=Online` before send-command, when in the up-window or a manual
    dispatch (self-heals a missed EventBridge start).
  - Tests: test_deploy_aws_self_starts_stopped_instance_and_skips_offhours_cleanly

- [x] **C — autopilot: widen self-start window to 08:30-16:30 IST**
  - Files: scripts/aws-autopilot.sh
  - Behaviour: new `is_box_up_window` (830-1630) replaces `is_market_window`
    for the stopped-box start, covering the 08:30-09:00 pre-market gap.
  - Tests: test_aws_autopilot_selfheal_workflow_exists

- [x] **Diagnostic (A) — autopilot: report why EventBridge start failed**
  - Files: scripts/aws-autopilot.sh
  - Behaviour: `diagnose_eventbridge_start` inspects the `tv-prod-daily-start`
    rule state + last `AWS-StartEC2Instance` automation when the box is found
    stopped in the up-window, surfacing the failing layer to the operator.
  - Tests: test_aws_autopilot_selfheal_workflow_exists

- [x] **Ratchet tests**
  - Files: crates/common/tests/aws_infra_wiring.rs
  - Tests: test_deploy_aws_self_starts_stopped_instance_and_skips_offhours_cleanly,
    test_aws_autopilot_selfheal_workflow_exists

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 08:30 EventBridge start fails; autopilot runs 08:30 | diagnoses + starts box |
| 2 | Push to main at 02:00 IST (box stopped) | clean skip, NO Telegram, run non-success |
| 3 | 08:45 pre-market cron, box stopped | deploy self-starts box + deploys |
| 4 | Manual workflow_dispatch any time, box stopped | self-starts box + deploys |
| 5 | Box already running | deploy proceeds unchanged |

## Verification

- `cargo test -p tickvault-common --test aws_infra_wiring` → 30 passed
- `cargo test -p tickvault-common --test github_workflow_guard` → push-trigger ratchet green
- `bash -n scripts/aws-autopilot.sh` → OK
- deploy-aws.yml parses as YAML
