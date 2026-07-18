# =============================================================================
# Budget Guards — Daily Telegram Digest + Hard Auto-Stop
# =============================================================================
# Two Lambdas that protect the operator's monthly budget:
#
# 1. tv-prod-daily-budget-digest (runs 17:30 IST Mon-Fri = 12:00 UTC)
#    - Queries Cost Explorer for today's spend + month-to-date
#    - Publishes Telegram-formatted message to SNS tv-prod-alerts
#    - Operator sees daily message: "Today ₹X | MTD ₹Y / ₹2000 (Z%)"
#
# 2. tv-prod-hard-stop-guard (runs HOURLY, every day)
#    - Force-stops the EC2 instance if running OUTSIDE the Mon-Fri
#      08:30-16:30 IST up-window (missed 16:30 stop / manual start)
#    - IN-window (GAP 1, 2026-07-03): if month-to-date spend has crossed
#      BUDGET_KILL_USD ($55 — same line as budget.tf limit_amount), it
#      stops the box AND disables the tv-prod-daily-start EventBridge
#      rule, because the native AWS Budget stop-actions fire only ONCE
#      per month-crossing — without this, the next morning's start cron
#      restarts the box and it runs daily, unkilled, for the rest of
#      the month. Cost Explorer errors fail-safe (page, never disable).
#    - Source lives in deploy/aws/lambda/hard-stop-guard/ (extracted
#      from an inline heredoc 2026-07-03 so the logic is unit-tested)
#
# Cost: Both Lambdas under 1 invocation/day each — well within the
# AWS Lambda free tier (1M invocations/mo). Zero additional cost.
# =============================================================================

# --------- Daily Budget Digest Lambda ---------
#
# 2026-07-18 (rust-only phase 2b-1): the inline Python heredoc was PORTED to
# Rust — crates/aws-lambdas/src/budget_digest.rs (lib logic + unit tests) +
# src/bin/daily_budget_digest.rs (thin bootstrap bin). Behavior parity:
# same Cost Explorer queries (us-east-1, DAILY UnblendedCost, exclusive
# end), same INR_PER_USD=85 / GST_MULT=1.18 / BUDGET_USD=55 constants
# (KEEP IN SYNC with budget.tf limit_amount), same emoji thresholds, same
# Telegram line format, same '[BUDGET] daily AWS cost' subject, same
# {'ok','mtd_usd','pct'} return. The zip is built in CI by the
# build-lambdas job (terraform-apply.yml) and downloaded into
# ${path.module}/.lambda-zips/ before plan/apply; source_code_hash is a
# digest of the Rust SOURCE (Rust builds are not bit-reproducible, so
# hashing the zip would churn every build with zero source change).
# 2026-07-18 (hostile-review r1 N3): a LOCAL operator `terraform plan`
# requires running the CI build step's cargo-lambda command first (the
# `file(".lambda-zips/source.digest")` reads fail otherwise) — CI-only
# planning is the intended contract; and a toolchain/cargo-lambda version
# bump alone does NOT change the digest (binaries redeploy on the next
# source / Cargo.lock change). Applies to all four .lambda-zips lambdas.

resource "aws_iam_role" "tv_daily_budget_digest" {
  name = "tv-prod-daily-budget-digest-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "tv_daily_budget_digest" {
  name = "tv-prod-daily-budget-digest-policy"
  role = aws_iam_role.tv_daily_budget_digest.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ce:GetCostAndUsage", "ce:GetCostForecast"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = "sns:Publish"
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        Effect   = "Allow"
        Action   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "*"
      },
    ]
  })
}

resource "aws_lambda_function" "tv_daily_budget_digest" {
  function_name    = "tv-prod-daily-budget-digest"
  filename         = "${path.module}/.lambda-zips/daily-budget-digest.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))
  role             = aws_iam_role.tv_daily_budget_digest.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  timeout          = 30
  memory_size      = 128
  environment {
    variables = {
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
    }
  }
}

resource "aws_cloudwatch_log_group" "tv_daily_budget_digest" {
  name              = "/aws/lambda/tv-prod-daily-budget-digest"
  retention_in_days = 14
}

resource "aws_cloudwatch_event_rule" "tv_daily_budget_digest" {
  name                = "tv-prod-daily-budget-digest"
  description         = "Run daily budget digest at 17:30 IST (12:00 UTC) Mon-Fri"
  schedule_expression = "cron(0 12 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_target" "tv_daily_budget_digest" {
  rule      = aws_cloudwatch_event_rule.tv_daily_budget_digest.name
  target_id = "tv-daily-budget-digest"
  arn       = aws_lambda_function.tv_daily_budget_digest.arn
}

resource "aws_lambda_permission" "tv_daily_budget_digest_eventbridge" {
  statement_id  = "AllowExecutionFromEventBridge"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_daily_budget_digest.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_daily_budget_digest.arn
}

# --------- Hard Auto-Stop Guard Lambda ---------

# Lambda source — packaged from deploy/aws/lambda/hard-stop-guard/
# (mirrors the budget-killswitch packaging idiom; the handler carries the
# GAP 1 breach->stop+disable logic with unit tests in test_handler.py).
data "archive_file" "tv_hard_stop_guard_zip" {
  type        = "zip"
  source_dir  = "${path.module}/../lambda/hard-stop-guard"
  output_path = "${path.module}/.tv-hard-stop-guard.zip"
  excludes    = ["README.md", "test_handler.py", "__pycache__", "*.pyc"]
}

resource "aws_iam_role" "tv_hard_stop_guard" {
  name = "tv-prod-hard-stop-guard-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "tv_hard_stop_guard" {
  name = "tv-prod-hard-stop-guard-policy"
  role = aws_iam_role.tv_hard_stop_guard.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ec2:DescribeInstances", "ec2:StopInstances"]
        Resource = "*"
      },
      {
        # Read-only MTD spend for the hourly cost ping AND the GAP 1
        # in-window breach check. Fail-safe: a Cost Explorer error never
        # stops the box or disables the start rule (page-only).
        Effect   = "Allow"
        Action   = ["ce:GetCostAndUsage"]
        Resource = "*"
      },
      {
        # GAP 1 (post-breach morning restart): on MTD >= $55 the Lambda
        # disables the morning start cron so the box cannot be restarted
        # daily for the rest of the month after a budget kill. Least
        # privilege — scoped to the ONE tv-prod-daily-start rule ARN.
        Sid      = "DisableDailyStartRuleOnBudgetBreach"
        Effect   = "Allow"
        Action   = ["events:DisableRule"]
        Resource = aws_cloudwatch_event_rule.daily_start.arn
      },
      {
        Effect   = "Allow"
        Action   = "sns:Publish"
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        # 2026-07-09 (Telegram noise N2 — change-only running pings): the
        # hourly in-window ping now fires only on a CHANGE (spend bucket /
        # month rollover / cost-check edge). State = ONE SSM String param.
        # Least privilege — scoped to that single parameter ARN (the
        # events:DisableRule idiom above); created lazily by
        # PutParameter(Overwrite=true), no seed resource needed. NOT under
        # the banned /tickvault/*/groww/* namespace.
        Sid      = "ChangeOnlyBudgetPingState"
        Effect   = "Allow"
        Action   = ["ssm:GetParameter", "ssm:PutParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:${data.aws_caller_identity.current.account_id}:parameter/tickvault/${var.environment}/budget-guard/ping-state"
      },
      {
        Effect   = "Allow"
        Action   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "*"
      },
    ]
  })
}

resource "aws_lambda_function" "tv_hard_stop_guard" {
  function_name    = "tv-prod-hard-stop-guard"
  filename         = data.archive_file.tv_hard_stop_guard_zip.output_path
  source_code_hash = data.archive_file.tv_hard_stop_guard_zip.output_base64sha256
  role             = aws_iam_role.tv_hard_stop_guard.arn
  handler          = "handler.lambda_handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  environment {
    variables = {
      INSTANCE_ID      = aws_instance.tv_app.id
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
      # GAP 1: the morning start cron the Lambda disables on a breach.
      START_RULE_NAME = aws_cloudwatch_event_rule.daily_start.name
      # KEEP IN SYNC with budget.tf limit_amount ("55") + the digest's
      # BUDGET_USD above — all three MUST agree on the kill line.
      BUDGET_KILL_USD = "55"
      # 2026-07-09: change-only ping state (matches the IAM statement's
      # single-parameter scope above).
      PING_STATE_PARAM = "/tickvault/${var.environment}/budget-guard/ping-state"
    }
  }
}

resource "aws_cloudwatch_log_group" "tv_hard_stop_guard" {
  name              = "/aws/lambda/tv-prod-hard-stop-guard"
  retention_in_days = 14
}

# Run HOURLY, every day (2026-06-30 — was once-daily 17:00 IST). The Lambda is
# window-aware: OUTSIDE the Mon-Fri 08:30-16:30 IST up-window it force-stops a
# running box (so a missed 16:30 stop or a manual start can NEVER bill a full
# overnight/weekend); INSIDE the window it checks the budget hourly but PINGS
# ONLY ON CHANGE (spend crossed a 10% bucket / month rollover / cost-check
# edge — 2026-07-09 Telegram noise N2; the 17:30 IST daily digest above stays
# the end-of-day summary), UNLESS MTD spend has crossed the $55 kill line —
# then it stops the box + disables the morning start cron (GAP 1, 2026-07-03).
# Hourly = the box can over-run the budget by at most ~1 EC2-hour (~$0.064)
# before this catches it, plus the native AWS Budget Action (budget.tf) stops
# at 90%/100% spend regardless (but only ONCE per month-crossing — this Lambda
# is what keeps the box down for the rest of the month).
# 1 invocation/hour is well within the Lambda free tier (1M/mo).
resource "aws_cloudwatch_event_rule" "tv_hard_stop_guard" {
  name                = "tv-prod-hard-stop-guard"
  description         = "Hourly out-of-window force-stop + in-window change-only budget ping — budget never-cross safety net"
  schedule_expression = "cron(0 * * * ? *)"
}

resource "aws_cloudwatch_event_target" "tv_hard_stop_guard" {
  rule      = aws_cloudwatch_event_rule.tv_hard_stop_guard.name
  target_id = "tv-hard-stop-guard"
  arn       = aws_lambda_function.tv_hard_stop_guard.arn
}

resource "aws_lambda_permission" "tv_hard_stop_guard_eventbridge" {
  statement_id  = "AllowExecutionFromEventBridge"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_hard_stop_guard.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_hard_stop_guard.arn
}
