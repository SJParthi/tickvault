# Budget kill-switch Lambda — Z+ L7 COOLDOWN layer.
#
# Wires the AWS Budgets 100% actual notification (already defined in
# budget.tf) to a dedicated SNS topic + Lambda that calls
# ec2:StopInstances on the tv-app instance. The Lambda also publishes
# a Severity::Critical message to the operator's regular tv_alerts
# topic so the Telegram webhook from PR #781 wakes the operator.
#
# Charter authority: operator-charter-forever.md §C row "100%
# alerting" + aws-budget.md §6 "EventBridge auto-start/stop". Without
# this Lambda, the 100% budget alarm is email-only — the operator may
# not see it for hours, during which the instance keeps burning EC2
# hours toward the next day's bill.
#
# Cost honesty:
#   - One additional SNS topic, one additional Lambda, one Lambda
#     self-error alarm, one log group.
#   - SNS first 1M publishes free; Lambda first 1M invocations free.
#   - 1 alarm above the 18 already added in PR #782 = $0.10/mo extra.
#   - Net: ~Rs 8/mo more. Budget total ~Rs 1,142 + Rs 8 = ~Rs 1,150.
#
# Safety design:
#   - Dedicated SNS topic (tv-${env}-budget-kill) — Lambda subscribes
#     ONLY here, NOT to tv_alerts. Prevents an unrelated CW alarm
#     from accidentally stopping the instance.
#   - IAM policy scopes ec2:StopInstances to the tv_app instance ARN
#     only. Lambda cannot stop any other EC2 instance in the account.
#   - Lambda's own Errors metric has a CloudWatch alarm that publishes
#     back to tv_alerts — if the kill-switch itself breaks, operator
#     learns via Telegram + email through the regular pipe.

# ---------------------------------------------------------------------------
# Dedicated SNS topic — budget breaches publish here, NOT tv_alerts
# ---------------------------------------------------------------------------

resource "aws_sns_topic" "tv_budget_kill" {
  name = "tv-${var.environment}-budget-kill"
  tags = {
    Project = "tickvault"
    Layer   = "L7-COOLDOWN"
  }
}

# AWS Budgets needs explicit permission to publish to the topic.
data "aws_iam_policy_document" "tv_budget_kill_topic_policy" {
  statement {
    sid     = "AllowBudgetsPublish"
    effect  = "Allow"
    actions = ["sns:Publish"]
    principals {
      type        = "Service"
      identifiers = ["budgets.amazonaws.com"]
    }
    resources = [aws_sns_topic.tv_budget_kill.arn]
  }
}

resource "aws_sns_topic_policy" "tv_budget_kill" {
  arn    = aws_sns_topic.tv_budget_kill.arn
  policy = data.aws_iam_policy_document.tv_budget_kill_topic_policy.json
}

# ---------------------------------------------------------------------------
# IAM role for the Lambda — minimum permissions
# ---------------------------------------------------------------------------

data "aws_iam_policy_document" "budget_killswitch_assume" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

data "aws_iam_policy_document" "budget_killswitch_permissions" {
  # Stop the tv-app instance ONLY. ARN-scoped so a compromised
  # Lambda can't stop other resources.
  statement {
    sid       = "Ec2StopTvAppOnly"
    effect    = "Allow"
    actions   = ["ec2:StopInstances"]
    resources = ["arn:aws:ec2:${var.aws_region}:*:instance/${aws_instance.tv_app.id}"]
  }

  # Publish the operator alert back to tv_alerts (existing topic
  # from main.tf — picked up by Telegram webhook Lambda from PR #781).
  statement {
    sid       = "SnsPublishToOperatorAlerts"
    effect    = "Allow"
    actions   = ["sns:Publish"]
    resources = [aws_sns_topic.tv_alerts.arn]
  }

  # Lambda's own CloudWatch Log stream.
  statement {
    sid    = "LambdaLogs"
    effect = "Allow"
    actions = [
      "logs:CreateLogGroup",
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    resources = ["arn:aws:logs:${var.aws_region}:*:*"]
  }
}

resource "aws_iam_role" "budget_killswitch" {
  name               = "tv-${var.environment}-budget-killswitch-lambda"
  assume_role_policy = data.aws_iam_policy_document.budget_killswitch_assume.json
}

resource "aws_iam_role_policy" "budget_killswitch" {
  name   = "tv-${var.environment}-budget-killswitch-policy"
  role   = aws_iam_role.budget_killswitch.id
  policy = data.aws_iam_policy_document.budget_killswitch_permissions.json
}

# ---------------------------------------------------------------------------
# Lambda source — Rust bin (rust-only phase 2b-1, 2026-07-18).
#
# The Python handler (deploy/aws/lambda/budget-killswitch/handler.py) was
# PORTED to Rust — crates/aws-lambdas/src/budget_killswitch.rs (lib logic;
# every python test ported to Rust unit tests) + src/bin/budget_killswitch.rs
# (thin bootstrap bin). Behavior parity: same SNS event parse (Records[0]
# .Sns.{Subject,Message}), same StopInstances on EC2_INSTANCE_ID, same
# Critical operator alert to ALERTS_TOPIC_ARN with the 99-char subject cap
# and 1000-char message truncation, same fail-loud re-raise so the
# budget_killswitch_errors self-error alarm below still fires on failure.
# The zip is built in CI by the build-lambdas job (terraform-apply.yml)
# into ${path.module}/.lambda-zips/ before plan/apply; source_code_hash is
# a digest of the Rust SOURCE (Rust builds are not bit-reproducible, so
# hashing the zip would churn every build with zero source change).
# ---------------------------------------------------------------------------

resource "aws_lambda_function" "budget_killswitch" {
  function_name    = "tv-${var.environment}-budget-killswitch"
  role             = aws_iam_role.budget_killswitch.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  timeout          = 30
  memory_size      = 128
  filename         = "${path.module}/.lambda-zips/budget-killswitch.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))

  environment {
    variables = {
      EC2_INSTANCE_ID  = aws_instance.tv_app.id
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
      LOG_LEVEL        = "INFO"
    }
  }

  tags = {
    Name    = "tv-${var.environment}-budget-killswitch"
    Project = "tickvault"
    Layer   = "L7-COOLDOWN"
  }
}

# ---------------------------------------------------------------------------
# CloudWatch Log Group — explicit retention + cleanup on destroy
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_log_group" "budget_killswitch" {
  name              = "/aws/lambda/tv-${var.environment}-budget-killswitch"
  retention_in_days = 30
  tags = {
    Project = "tickvault"
    Layer   = "L7-COOLDOWN"
  }
}

# ---------------------------------------------------------------------------
# SNS subscription — budget-kill topic to the Lambda
# ---------------------------------------------------------------------------

resource "aws_lambda_permission" "budget_killswitch_sns" {
  statement_id  = "AllowExecutionFromSnsBudgetKill"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.budget_killswitch.function_name
  principal     = "sns.amazonaws.com"
  source_arn    = aws_sns_topic.tv_budget_kill.arn
}

resource "aws_sns_topic_subscription" "budget_killswitch" {
  topic_arn = aws_sns_topic.tv_budget_kill.arn
  protocol  = "lambda"
  endpoint  = aws_lambda_function.budget_killswitch.arn
}

# ---------------------------------------------------------------------------
# Lambda self-error alarm — if THIS Lambda errors, the budget cap is
# offline. Operator must be paged through the regular tv_alerts pipe.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "budget_killswitch_errors" {
  alarm_name          = "tv-${var.environment}-budget-killswitch-errors"
  alarm_description   = "Budget kill-switch Lambda is failing — the 100% budget cap is OFFLINE. Investigate /aws/lambda/tv-${var.environment}-budget-killswitch immediately."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"

  dimensions = {
    FunctionName = aws_lambda_function.budget_killswitch.function_name
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# Outputs
# ---------------------------------------------------------------------------

output "budget_killswitch_lambda_name" {
  description = "Name of the budget kill-switch Lambda function."
  value       = aws_lambda_function.budget_killswitch.function_name
}

output "budget_killswitch_topic_arn" {
  description = "Dedicated SNS topic ARN — AWS Budgets publishes here at 100% breach. Wire it via budget.tf's notification.subscriber_sns_topic_arns."
  value       = aws_sns_topic.tv_budget_kill.arn
}
