# =============================================================================
# Market-open readiness pager Lambda — daily-universe §19, implemented 2026-07-06
# =============================================================================
# THE GAP THIS CLOSES (zero-page incident leg 2): on 2026-07-06 the box
# launched at 10:04 IST (94 min late); BOTH liveness alarms were latched in
# ALARM from the weekend, so there was no OK->ALARM edge in the actionable
# window -> zero pages. This Lambda is a STATE-check, not an edge-check: at
# 08:45 IST it evaluates raw reality (EC2 state + tv_boot_completed presence)
# and publishes DIRECTLY to SNS on a bad answer — it cannot be defeated by
# alarm latch history. Implements daily-universe-scope-expansion-2026-05-27.md
# §19 (specified 08:40, never implemented) at the operator-directed 08:45 IST.
#
# DELIBERATE cron collision with start_watchdog_check (same
# cron(15 3 ? * MON-FRI *), start-watchdog-lambda.tf): when EC2 is down at
# 08:45 BOTH page — the watchdog says "I started it myself" (it has
# StartInstances), readiness says "app will not be ready — watch for the
# started message". Two different actionable statements at T-30 min; on the
# common bad shape (box up late / app hung — the 10:04 case) only the
# readiness pager fires, which is the closed gap.
#
# SEPARATION OF DUTIES: NO ec2:StartInstances here — this Lambda PAGES; the
# start-watchdog HEALS.
# =============================================================================

# ---------------------------------------------------------------------------
# IAM — assume + least-privilege (describe EC2 + read metrics + publish + logs)
# ---------------------------------------------------------------------------

resource "aws_iam_role" "market_open_readiness" {
  name = "tv-${var.environment}-market-open-readiness-lambda"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "market_open_readiness" {
  name = "tv-${var.environment}-market-open-readiness-policy"
  role = aws_iam_role.market_open_readiness.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # DescribeInstances has no resource-level scoping in IAM (AWS limitation),
        # so "*" is required. The Lambda only ever reads EC2_INSTANCE_ID.
        Effect   = "Allow"
        Action   = ["ec2:DescribeInstances"]
        Resource = "*"
      },
      {
        # GetMetricData supports NO resource scoping and NO namespace condition
        # key (that key exists only for PutMetricData), so "*" is required.
        # The Lambda only ever reads the single tv_boot_completed query.
        Effect   = "Allow"
        Action   = ["cloudwatch:GetMetricData"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = ["sns:Publish"]
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        Effect = "Allow"
        Action = [
          "logs:CreateLogGroup",
          "logs:CreateLogStream",
          "logs:PutLogEvents",
        ]
        Resource = "arn:aws:logs:${var.aws_region}:*:*"
      }
    ]
  })
}

# ---------------------------------------------------------------------------
# Lambda source — packaged from deploy/aws/lambda/market-open-readiness/
# ---------------------------------------------------------------------------

data "archive_file" "market_open_readiness" {
  type        = "zip"
  source_dir  = "${path.module}/../lambda/market-open-readiness"
  output_path = "${path.module}/.market-open-readiness.zip"
  excludes    = ["README.md", "test_handler.py", "__pycache__", "*.pyc"]
}

resource "aws_lambda_function" "market_open_readiness" {
  function_name    = "tv-${var.environment}-market-open-readiness"
  role             = aws_iam_role.market_open_readiness.arn
  handler          = "handler.lambda_handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  filename         = data.archive_file.market_open_readiness.output_path
  source_code_hash = data.archive_file.market_open_readiness.output_base64sha256

  environment {
    variables = {
      EC2_INSTANCE_ID       = aws_instance.tv_app.id
      ALERTS_TOPIC_ARN      = aws_sns_topic.tv_alerts.arn
      METRIC_NAMESPACE      = "Tickvault/Prod"
      BOOT_METRIC_NAME      = "tv_boot_completed"
      METRIC_HOST           = "tickvault-prod"
      BOOT_LOOKBACK_MINUTES = "10"
      LOG_LEVEL             = "INFO"
    }
  }

  tags = {
    Name    = "tv-${var.environment}-market-open-readiness"
    Project = "tickvault"
    Layer   = "L1-DETECT"
  }
}

resource "aws_cloudwatch_log_group" "market_open_readiness" {
  name              = "/aws/lambda/tv-${var.environment}-market-open-readiness"
  retention_in_days = 30
  tags = {
    Project = "tickvault"
    Layer   = "L1-DETECT"
  }
}

# ---------------------------------------------------------------------------
# EventBridge schedule — 08:45 IST (03:15 UTC) Mon-Fri
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_event_rule" "market_open_readiness" {
  name                = "tv-${var.environment}-market-open-readiness"
  description         = "08:45 IST (Mon-Fri) market-open readiness pager - daily-universe-scope-expansion §19 (state-check; immune to latched alarms - 2026-07-06 incident)"
  schedule_expression = "cron(15 3 ? * MON-FRI *)"
  # 2026-07-06: explicit state so terraform manages rule state (the Jul-4
  # pause/#1404 incident class - a rule WITHOUT the attribute is not
  # state-managed by the provider).
  state = "ENABLED"
}

resource "aws_cloudwatch_event_target" "market_open_readiness" {
  rule      = aws_cloudwatch_event_rule.market_open_readiness.name
  target_id = "tv-market-open-readiness"
  arn       = aws_lambda_function.market_open_readiness.arn
  input     = jsonencode({ mode = "readiness" })
}

resource "aws_lambda_permission" "market_open_readiness" {
  statement_id  = "AllowExecutionFromEventBridgeReadiness"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.market_open_readiness.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.market_open_readiness.arn
}

# ---------------------------------------------------------------------------
# Watch the watchman — the readiness pager itself failing must page.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "market_open_readiness_lambda_errors" {
  alarm_name          = "tv-${var.environment}-market-open-readiness-errors"
  alarm_description   = "The 08:45 IST readiness pager itself FAILED - the watchman must be watched (the pager exists because silent failure is the incident class). Its SNS-publish path deliberately RAISES on failure so a broken page lands here instead of dying silently."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.market_open_readiness.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = [aws_sns_topic.tv_alerts.arn]
}
