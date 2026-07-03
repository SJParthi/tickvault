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
