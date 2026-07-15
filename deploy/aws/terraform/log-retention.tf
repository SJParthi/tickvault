# =============================================================================
# Log retention + ingestion runaway guard (GAP 3 of the budget-net audit)
# =============================================================================
# The 2026-07-03 audit found TWO live log groups with NO retention policy
# (grow-forever, unbounded storage cost):
#
#   1. /aws/lambda/tv-prod-operator-control — auto-created by the Lambda
#      runtime on first invocation (the operator-control-lambda.tf module
#      never declared an explicit log group, unlike every other Lambda).
#   2. /tickvault/prod/metrics — auto-created by the CloudWatch agent on
#      the box (user-data.sh.tftpl); metrics-log-metric-filters.tf reads
#      it by name but never managed it.
#
# Both are adopted here with the stack-standard 14-day retention (the same
# value as /tickvault/prod/app + the budget-guard Lambdas). The `import`
# blocks adopt the EXISTING live groups so `terraform apply` does not fail
# with ResourceAlreadyExistsException; import blocks are idempotent (no-op
# once the resource is in state). On a FRESH account where these groups do
# not yet exist, delete the two import blocks and terraform creates them.
# =============================================================================

import {
  to = aws_cloudwatch_log_group.operator_control
  id = "/aws/lambda/tv-${var.environment}-operator-control"
}

resource "aws_cloudwatch_log_group" "operator_control" {
  name              = "/aws/lambda/tv-${var.environment}-operator-control"
  retention_in_days = 14
}

import {
  to = aws_cloudwatch_log_group.tv_metrics
  id = "/tickvault/${var.environment}/metrics"
}

resource "aws_cloudwatch_log_group" "tv_metrics" {
  name              = "/tickvault/${var.environment}/metrics"
  retention_in_days = 14
}

# ---------------------------------------------------------------------------
# Account-level CloudWatch Logs ingestion alarm — catches ANY log runaway
# (a debug-loop Lambda, a chatty agent scrape, a new unretained group)
# before it becomes a bill: CloudWatch Logs ingestion is $0.67/GB after the
# 5 GB/mo free tier, so a silent 5 GB/day runaway ≈ $100/mo — double the
# entire $55 stop-budget. AWS/Logs IncomingBytes with NO dimensions is the
# account-wide aggregate across every log group.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "logs_ingestion_runaway" {
  alarm_name          = "tv-${var.environment}-logs-ingestion-runaway"
  alarm_description   = "CloudWatch Logs ingested > ~5 GB in 24h across the account — a log runaway is burning the budget. Find the offender: aws logs describe-log-groups --query 'sort_by(logGroups,&storedBytes)[-5:]'"
  namespace           = "AWS/Logs"
  metric_name         = "IncomingBytes"
  statistic           = "Sum"
  period              = 86400 # 1 day — threshold is a per-day byte budget
  evaluation_periods  = 1
  threshold           = 5368709120 # 5 GiB/day
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# App log-group ingestion SILENCE alarm — the inverse of the runaway guard.
# Incident 2026-07-06 17:19 IST: the app moved its machine logs to
# data/logs/machine/ (the "one human log file" reorg) while the CloudWatch
# agent kept tailing the OLD top-level globs — the app stayed healthy, the
# shipper went dead, and /tickvault/prod/app ingested NOTHING with zero
# operator signal (every value-based app alarm kept evaluating its metric;
# nothing watched ingestion itself). This alarm pages when the app log group
# ingests ZERO events for ~15 minutes during market hours.
#
# AWS/Logs publishes NO IncomingLogEvents datapoint for a period with zero
# ingestion — silence IS missing data — so treat_missing_data must be
# "breaching" for this alarm to detect anything at all. That makes it a
# guaranteed false-pager while the box is intentionally stopped (nightly /
# weekend / weekday-NSE-holiday self-stop via holiday-gate.sh), so actions are
# OFF by default and window-gated: the market-hours gate Lambda
# (market-hours-liveness-alarm.tf) enables them 09:20-15:35 IST Mon-Fri —
# and, per the 2026-07-07 review fix, ONLY after verifying the tv-app
# instance is actually up (the open cron is holiday-blind; the box is OFF on
# weekday NSE holidays, so a blind enable would false-page ~09:35 IST every
# holiday). This alarm is the 2nd member of the 4-alarm ALARM_NAMES list
# (ordinal corrected 2026-07-15 after the PR-C2/C3 + Groww live-feed
# retirements trimmed the list). The deploy
# workflow's LOG-INGESTION-SMOKE step is the independent per-deploy detector
# for the same failure class.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "app_log_ingestion_silent" {
  alarm_name          = "tv-${var.environment}-app-log-ingestion-silent"
  alarm_description   = "App log shipping is SILENT: /tickvault/${var.environment}/app ingested ZERO log events for ~15 min during market hours. The app itself may be perfectly healthy — the 2026-07-06 incident signature is the on-box CloudWatch agent tailing globs that no longer match the app's log files (data/logs/machine/ reorg). Triage via SSM on the box: 'sudo /opt/aws/amazon-cloudwatch-agent/bin/amazon-cloudwatch-agent-ctl -a status' + 'sudo tail -n 40 /opt/aws/amazon-cloudwatch-agent/logs/amazon-cloudwatch-agent.log' + 'ls -la /opt/tickvault/data/logs/machine/' — the agent's collect_list globs must match deploy/aws/cloudwatch-agent.json (machine/errors.jsonl.2* + machine/app.2*)."
  namespace           = "AWS/Logs"
  metric_name         = "IncomingLogEvents"
  statistic           = "Sum"
  period              = 300 # 5 min × 3 evaluation periods = ~15 min of silence
  evaluation_periods  = 3
  threshold           = 1
  comparison_operator = "LessThanThreshold"
  treat_missing_data  = "breaching" # INTENTIONAL inverse of the runaway guard — zero ingestion = no datapoint

  dimensions = {
    LogGroupName = aws_cloudwatch_log_group.tv_app.name
  }

  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri — only when the box is actually up — so the
  # intentional nightly/weekend/NSE-holiday stop (box down => zero ingestion
  # => missing datapoints) can never false-page.
  actions_enabled = false
  alarm_actions   = [aws_sns_topic.tv_alerts.arn]
  ok_actions      = [aws_sns_topic.tv_alerts.arn]
}
