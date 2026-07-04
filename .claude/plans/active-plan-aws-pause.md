# Implementation Plan: AWS pause — no auto-start Mon Jul 6 – Wed Jul 8 2026 (operator-directed local-only window)

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — verbatim quotes below, 2026-07-04

> **Operator authorization (2026-07-04, verbatim):**
> "yes i think this would be entirely better... instead of struggling between aws vs local its completely better to just go ahead with only local... at least for three days starting monday till wednesday will you ensure it is entirely off in aws dude no auto start dude"
>
> Earlier same-day quote proposing it:
> "how about as of now stopping the entire aws and just purely work on local at least for this monday till wednesday to verify configure everything"
>
> Accepted consequence (told to the operator in-session): the weekday liveness watchdog alarms are muted for the pause window so he doesn't get 3 days of false "box is dead" Telegram pages. Normal schedule resumes Thu Jul 9 08:30 IST via the pre-staged revert branch `claude/aws-resume-jul9`.

## Design

Minimal + loud terraform-only change: flip the EventBridge rules that (a) auto-START the prod box or (b) would false-page while the box is deliberately OFF, to `state = "DISABLED"`, each carrying a dated `# PAUSED 2026-07-04 ... MUST BE REVERTED for Thu Jul 9` comment. Everything that STOPS the box stays enabled as a harmless safety net.

| Rule (deploy/aws/terraform/) | File | Change | Why |
|---|---|---|---|
| `daily_start` (`tv-prod-daily-start`, cron 03:00 UTC Mon-Fri) | main.tf | DISABLED | the 08:30 IST auto-start — the core of the pause |
| `daily_stop` | main.tf | UNCHANGED (enabled) | stop is a harmless safety net |
| `start_watchdog_ping` (08:30 IST) | start-watchdog-lambda.tf | DISABLED | would falsely announce "start triggered" |
| `start_watchdog_check` (08:45 IST) | start-watchdog-lambda.tf | DISABLED | its self-heal issues `ec2:StartInstances` — would UNDO the pause, then page about a "failed start" |
| `start_watchdog_stop_check` + `start_watchdog_curfew_check` | start-watchdog-lambda.tf | UNCHANGED | stop-only actions; silent no-ops on a stopped box |
| `tv_boot_heartbeat_open` (08:50 IST gate) | boot-heartbeat-alarm.tf | DISABLED | `tv-prod-boot-heartbeat-missing` is `actions_enabled=false` by default; never opening the gate means it can never page during the pause. Close gate (09:10 IST) stays enabled — harmless re-assert of actions-off |
| `tv_market_hours_liveness_open` (09:20 IST gate) | market-hours-liveness-alarm.tf | DISABLED | same gate pattern — mutes `tv-prod-market-hours-liveness-missing` plus the two value-based gated alarms (`realtime-guarantee-critical`, `aggregator-no-seals`) that share the gate Lambda. Close gate (15:35 IST) stays enabled |
| `deploy_watchdog_premarket` (08:50 IST) + `deploy_watchdog_postmarket` (15:51 IST) | deploy-watchdog-lambda.tf | DISABLED | this PR moves main HEAD, so the stale-check would page HIGH "cron slipped" every day AND dispatch `deploy-aws-after-close.yml`, whose `deploy-aws.yml` run SELF-STARTS a stopped box |
| `deploy_watchdog_instance_start` (EC2 running event) | deploy-watchdog-lambda.tf | UNCHANGED | only fires if the box actually starts; a deliberate MANUAL operator start should still get an instant deploy |

Why gate-rule-disable instead of `actions_enabled = false` on the alarms: both liveness alarms are ALREADY `actions_enabled = false` in terraform; the live enable comes exclusively from the window-gate Lambda fired by the `*_open` rules. Disabling the open rules is the single smallest change that silences them, needs no alarm-resource edit, and the still-enabled `*_close` rules re-assert actions-off daily as defense in depth.

GitHub-Actions self-start paths (guarded post-merge via GitHub's NATIVE workflow enable/disable toggle — no new machinery): `aws-autopilot.yml` (cron every 15 min 08:30–16:30 IST Mon-Fri; `scripts/aws-autopilot.sh` runs `ec2 start-instances` on any stopped box inside the up-window), `deploy-aws-after-close.yml` (5 weekday crons; would see main moved and dispatch deploy-aws), and `deploy-aws.yml` (`Ensure instance running` step self-starts a stopped box inside the up-window or on any workflow_dispatch). All three are disabled with `gh workflow disable` for the window and MUST be re-enabled (`gh workflow enable`) on Thu Jul 9 before 08:30 IST alongside the terraform revert.

## Edge Cases

- **Operator manually starts the box Mon-Wed:** allowed and unaffected — the pause removes only AUTO-start. The curfew guard + hourly hard-stop guard remain enabled and will auto-stop a forgotten out-of-hours box; `deploy_watchdog_instance_start` still gives a manual start an instant deploy ONLY if deploy workflows are re-enabled (noted in PR body).
- **Other PRs merge Mon-Wed:** deploy-aws push runs are suppressed by the workflow disable; commits accumulate and deploy Thursday at the 08:30 after-close cron once workflows are re-enabled.
- **terraform-apply on Saturday:** market-hours guard passes (weekend); bot-merge push suppression means the apply must be dispatched manually (workflow_dispatch) — done as part of this task.
- **Alarm currently in ALARM state:** actions are disabled outside windows (Friday close gates already ran), and the open-gate Lambda resets state to OK on open — Thursday's first open resets any stale ALARM.
- **NSE holiday inside the window:** irrelevant — box stays off either way.

## Failure Modes

- **terraform-apply fails post-merge:** rules stay ENABLED in AWS → the box would start Monday as usual (fail-safe toward the OLD behavior, no data loss). Mitigation: dispatch + watch the apply to SUCCESS on Saturday, then verify via `aws events describe-rule` (done in this task).
- **A missed self-start path restarts the box:** the enumerated paths (EventBridge start, start-watchdog self-heal, autopilot, after-close→deploy-aws chain, deploy-watchdog dispatch) are all disabled; the remaining hourly curfew guard would stop an out-of-hours box, and 08:30–16:30 in-window restart requires one of the disabled paths. Residual risk: a human workflow_dispatch — operator-controlled by definition.
- **Revert forgotten Thursday:** the box never auto-starts again and the operator gets NO liveness page telling him so (alarms muted). Mitigation: pre-staged `claude/aws-resume-jul9` branch + loud `MUST BE REVERTED` comments on every changed resource + explicit resume checklist in the PR body.

## Test Plan

- `terraform fmt -check` — PASS (run locally with terraform 1.9.8).
- `terraform init -backend=false` + `terraform validate` — PASS (aws provider 5.100.0 within `~> 5.80`; `state` attribute valid).
- Repo ratchet tests that pin terraform contents (no pinned expectation changes needed — cron strings and resource structure are untouched; only `state` + comments added):
  - `cargo test -p tickvault-common --test aws_infra_wiring --test aws_alarm_semantics_guard --test cloudwatch_app_alarms_wiring --test budget_killswitch_wiring --test deploy_provenance_guard --test github_workflow_guard --test claude_triage_lambda_guard`
  - `cargo test -p tickvault-app --test boot_completed_metric_guard --test close_pct_realtime_proof_guard`
  - `cargo test -p tickvault-storage --test aws_deploy_safety_guard --test instance_type_lock_guard`
- Post-apply AWS verification: `aws events describe-rule --name tv-prod-daily-start` shows `State: DISABLED`; same for the ping/check/gate/deploy-watchdog rules; both liveness alarms show `ActionsEnabled: false`.

## Rollback

Pre-staged branch `claude/aws-resume-jul9` = exact revert commit of this change (`chore(aws): resume weekday auto-start from Thu Jul 9 (revert pause window)`). To resume: open + merge that branch's PR before Thu Jul 9 08:30 IST (03:00 UTC), ensure terraform-apply runs to SUCCESS, and re-enable the three GitHub workflows (`gh workflow enable aws-autopilot.yml deploy-aws-after-close.yml deploy-aws.yml` — one call each). Emergency same-day rollback: `aws events enable-rule --name tv-prod-daily-start` (and siblings) restores behavior instantly without terraform; the next terraform-apply would re-disable until the revert merges, so prefer the revert PR.

## Observability

- Every changed resource carries the dated `# PAUSED 2026-07-04 ... MUST BE REVERTED` comment — greppable (`grep -rn "PAUSED 2026-07-04" deploy/aws/terraform/`).
- The terraform-apply run output (Telegram-notified per that workflow) shows the 7 rule updates.
- Post-apply CLI verification pasted verbatim in the task report (describe-rule + describe-alarms).
- During the window the operator deliberately receives NO box-liveness Telegram (accepted consequence, quoted above); budget digest + curfew/stop guards remain live.

## Plan Items

- [x] Disable `daily_start` EventBridge rule (keep `daily_stop`)
  - Files: deploy/aws/terraform/main.tf
  - Tests: test_eventbridge_daily_start_stop_rules (aws_infra_wiring — kept green)
- [x] Disable start-watchdog ping + check rules (keep stop_check + curfew_check)
  - Files: deploy/aws/terraform/start-watchdog-lambda.tf
  - Tests: test_start_watchdog_lambda_monitors_the_morning_start (kept green)
- [x] Disable boot-heartbeat + market-hours-liveness OPEN gate rules (keep CLOSE gates)
  - Files: deploy/aws/terraform/boot-heartbeat-alarm.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf
  - Tests: test_boot_heartbeat_alarm_contract_unchanged, boot_completed_metric_guard (kept green)
- [x] Disable deploy-watchdog premarket + postmarket cron rules (keep instance-start event rule)
  - Files: deploy/aws/terraform/deploy-watchdog-lambda.tf
  - Tests: test_deploy_watchdog_lambda_is_wired (kept green)
- [x] Validate: terraform fmt -check + validate green; terraform-pinning ratchet tests green
  - Files: (none — verification)
  - Tests: aws_infra_wiring, aws_alarm_semantics_guard, boot_completed_metric_guard, aws_deploy_safety_guard, instance_type_lock_guard

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Mon Jul 6 03:00 UTC | daily_start DISABLED — box stays stopped; no start-watchdog ping/check; no boot-heartbeat page |
| 2 | Mon-Wed 09:20-15:35 IST | liveness gate never opens — no "box is dead" Telegram despite missing metrics |
| 3 | Mon 08:50 IST | deploy-watchdog premarket DISABLED — no HIGH "cron slipped" page, no dispatch chain |
| 4 | Operator manually starts box Tue | allowed; curfew guard + hard-stop guard still protect against a forgotten box |
| 5 | Thu Jul 9 03:00 UTC (post-revert) | daily_start ENABLED again — normal 08:30 IST start, watchdogs + alarms restored |

## Guarantee matrices

Per-item 15-row + 7-row guarantee matrices: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` — this is an infra-only (terraform + workflow-toggle) change with zero Rust/hot-path/QuestDB surface; the applicable rows are covered by the Test Plan (ratchet tests kept green, terraform validate) and Observability sections above; all code-coverage / DHAT / DEDUP / hot-path rows are N/A — no application code touched.
