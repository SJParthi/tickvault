# Deploy-watchdog Lambda — AWS-native safety-net for the post-merge auto-deploy.
#
# Operator decision 2026-06-02 ("AWS watchdog safety-net"): keep the existing
# GitHub-Actions auto-deploy (deploy-aws-after-close.yml cron @ 08:00 + 08:45 +
# 15:46 IST), but add an AWS-native check a few minutes later that covers GitHub-cron
# misses (GitHub-hosted cron can be silently delayed/skipped under load).
#
# Two EventBridge schedules invoke the same Lambda with a `window` input:
#   * 08:50 IST = 03:20 UTC Mon-Fri — 5 min after the pre-market cron.
#   * 15:51 IST = 10:21 UTC Mon-Fri — 5 min after the 15:46 post-market cron.
#
# The Lambda asks GitHub "is main HEAD already deployed?" (deployed = head_sha of
# the most recent SUCCESSFUL deploy-aws.yml run — the same idempotency signal the
# cron uses). If stale, it dispatches deploy-aws-after-close.yml (idempotent +
# market-hours-guarded, so a double-fire with the cron is a safe no-op) and pages
# the operator. If healthy OR uncertain, it stays silent.
#
# Cost: 2 invokes/weekday (~44/mo) — free tier (1M req/mo). Effectively ₹0.

# ---------------------------------------------------------------------------
# IAM — assume + least-privilege (read github-token from SSM + publish + logs)
# ---------------------------------------------------------------------------

resource "aws_iam_role" "deploy_watchdog" {
  name = "tv-${var.environment}-deploy-watchdog-lambda"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "deploy_watchdog" {
  name = "tv-${var.environment}-deploy-watchdog-policy"
  role = aws_iam_role.deploy_watchdog.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # Repo-scoped GitHub token (same param the operator-control Lambda
        # reads). Used only to read main HEAD + last-deploy sha and to
        # workflow_dispatch the auto-deploy workflow.
        Effect   = "Allow"
        Action   = ["ssm:GetParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/operator/github-token"
      },
      {
        Effect   = "Allow"
        Action   = ["sns:Publish"]
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        # B9 deploy provenance: read the deployed-binary git SHA (written
        # by deploy-aws.yml after a verified swap) to compare against main
        # HEAD for the tv_binary_main_sha_mismatch metric.
        Effect   = "Allow"
        Action   = ["ssm:GetParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/deploy/binary-git-sha"
      },
      {
        # B9 deploy provenance: publish the mismatch metric. PutMetricData
        # supports no resource-level scoping — constrain by namespace so
        # this Lambda can only ever write Tickvault/Prod metrics.
        Effect   = "Allow"
        Action   = ["cloudwatch:PutMetricData"]
        Resource = "*"
        Condition = {
          StringEquals = { "cloudwatch:namespace" = "Tickvault/Prod" }
        }
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
# Lambda source — packaged from deploy/aws/lambda/deploy-watchdog/
# ---------------------------------------------------------------------------

data "archive_file" "deploy_watchdog" {
  type        = "zip"
  source_dir  = "${path.module}/../lambda/deploy-watchdog"
  output_path = "${path.module}/.deploy-watchdog.zip"
  excludes    = ["README.md", "test_handler.py", "__pycache__", "*.pyc"]
}

resource "aws_lambda_function" "deploy_watchdog" {
  function_name    = "tv-${var.environment}-deploy-watchdog"
  role             = aws_iam_role.deploy_watchdog.arn
  handler          = "handler.lambda_handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  filename         = data.archive_file.deploy_watchdog.output_path
  source_code_hash = data.archive_file.deploy_watchdog.output_base64sha256

  environment {
    variables = {
      GH_REPO                     = var.operator_github_repo
      GH_DESIRED_REF              = "main"
      GH_DEPLOY_WORKFLOW          = "deploy-aws.yml"
      GH_DISPATCH_WORKFLOW        = "deploy-aws-after-close.yml"
      OPERATOR_GITHUB_TOKEN_PARAM = "/tickvault/${var.environment}/operator/github-token"
      ALERTS_TOPIC_ARN            = aws_sns_topic.tv_alerts.arn
      LOG_LEVEL                   = "INFO"
      # B9 deploy provenance: deployed-binary sha param (deploy-aws.yml writes it).
      # LOCKSTEP NOTE: deploy-aws.yml WRITES the param at the hardcoded
      # /tickvault/prod/... path (prod is the only real env) — the two must
      # stay in lockstep; a staging env would need the workflow parameterized.
      BINARY_SHA_PARAM = "/tickvault/${var.environment}/deploy/binary-git-sha"
    }
  }

  tags = {
    Name    = "tv-${var.environment}-deploy-watchdog"
    Project = "tickvault"
    Layer   = "L6-RECOVER"
  }
}

resource "aws_cloudwatch_log_group" "deploy_watchdog" {
  name              = "/aws/lambda/tv-${var.environment}-deploy-watchdog"
  retention_in_days = 30
  tags = {
    Project = "tickvault"
    Layer   = "L6-RECOVER"
  }
}

# ---------------------------------------------------------------------------
# EventBridge schedules — 5 min after each GitHub deploy cron, weekdays
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_event_rule" "deploy_watchdog_premarket" {
  name                = "tv-${var.environment}-deploy-watchdog-premarket"
  description         = "08:50 IST (Mon-Fri) — cover a missed 08:45 pre-market auto-deploy"
  schedule_expression = "cron(20 3 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_rule" "deploy_watchdog_postmarket" {
  name                = "tv-${var.environment}-deploy-watchdog-postmarket"
  description         = "15:51 IST (Mon-Fri) — cover a missed 15:46 post-market auto-deploy"
  schedule_expression = "cron(21 10 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_target" "deploy_watchdog_premarket" {
  rule      = aws_cloudwatch_event_rule.deploy_watchdog_premarket.name
  target_id = "deploy-watchdog-premarket"
  arn       = aws_lambda_function.deploy_watchdog.arn
  input     = jsonencode({ window = "pre-market" })
}

resource "aws_cloudwatch_event_target" "deploy_watchdog_postmarket" {
  rule      = aws_cloudwatch_event_rule.deploy_watchdog_postmarket.name
  target_id = "deploy-watchdog-postmarket"
  arn       = aws_lambda_function.deploy_watchdog.arn
  input     = jsonencode({ window = "post-market" })
}

resource "aws_lambda_permission" "deploy_watchdog_premarket" {
  statement_id  = "AllowExecutionFromEventBridgePremarket"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.deploy_watchdog.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.deploy_watchdog_premarket.arn
}

resource "aws_lambda_permission" "deploy_watchdog_postmarket" {
  statement_id  = "AllowExecutionFromEventBridgePostmarket"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.deploy_watchdog.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.deploy_watchdog_postmarket.arn
}

# ---------------------------------------------------------------------------
# INSTANT deploy on instance start (operator directive 2026-06-02)
#
# "auto deployment should happen instantly when the instance is started at
# 08:00." The 08:00 EventBridge start brings the box up; this rule fires the
# deploy-watchdog the MOMENT the instance enters the `running` state — so the
# latest `main` is pulled + restarted within a couple of minutes of 08:00,
# NOT at the 08:45 cron. The watchdog is idempotent (only dispatches if main
# is actually newer than the last successful deploy), so a same-day restart
# that is already up-to-date is a silent no-op. The 08:45/08:50 schedules
# above remain as belt-and-suspenders backups.
#
# Event-pattern (not a schedule): EC2 Instance State-change → `running` for
# THIS instance only. SSM agent is up within ~30-60s of `running`; the
# dispatched GitHub deploy takes ~1-2 min to reach its SSM step, by which the
# box is ready.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_event_rule" "deploy_watchdog_instance_start" {
  name        = "tv-${var.environment}-deploy-watchdog-instance-start"
  description = "Instant deploy when the tv-app EC2 instance enters 'running' (08:00 auto-start)"
  event_pattern = jsonencode({
    source        = ["aws.ec2"]
    "detail-type" = ["EC2 Instance State-change Notification"]
    detail = {
      state         = ["running"]
      "instance-id" = [aws_instance.tv_app.id]
    }
  })
}

resource "aws_cloudwatch_event_target" "deploy_watchdog_instance_start" {
  rule      = aws_cloudwatch_event_rule.deploy_watchdog_instance_start.name
  target_id = "deploy-watchdog-instance-start"
  arn       = aws_lambda_function.deploy_watchdog.arn
  input     = jsonencode({ window = "instance-start" })
}

resource "aws_lambda_permission" "deploy_watchdog_instance_start" {
  statement_id  = "AllowExecutionFromEventBridgeInstanceStart"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.deploy_watchdog.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.deploy_watchdog_instance_start.arn
}

# ---------------------------------------------------------------------------
# B9 deploy provenance — binary-vs-main staleness alarm
#
# The watchdog publishes tv_binary_main_sha_mismatch (1.0 = the deployed
# binary's SHA differs from main HEAD, 0.0 = in sync; SKIPPED when either
# SHA is unknown) on every fire — ~2-3 samples/weekday (instance-start +
# 08:50 + 15:51 IST crons).
#
# Alarm semantics: statistic Minimum over ONE 86400s (24h) period >= 1
# means EVERY sample in the day reported mismatch — i.e. the box ran stale
# code across the whole day's watchdog samples. At the ~2-3 samples/day
# cadence this APPROXIMATES ">24h stale" (documented honestly — it is not
# an exact wall-clock measurement). treat_missing_data = notBreaching:
# box off / weekends / unknown-sha skips never page.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "binary_sha_stale" {
  alarm_name          = "tv-${var.environment}-binary-sha-stale"
  alarm_description   = "Running binary git SHA has differed from main HEAD for a full 24h window — deploy pipeline is stuck or bypassed; runbook: check deploy-aws.yml runs + SSM /tickvault/prod/deploy/binary-git-sha (see .claude/rules/project/deploy-provenance.md)"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_binary_main_sha_mismatch"
  namespace           = "Tickvault/Prod"
  period              = 86400
  statistic           = "Minimum"
  threshold           = 1
  dimensions          = { host = "tickvault-prod" }
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.tv_alerts.arn]
  ok_actions          = [aws_sns_topic.tv_alerts.arn]

  tags = {
    Project = "tickvault"
    Layer   = "L2-VERIFY"
  }
}

output "deploy_watchdog_function_name" {
  description = "Deploy-watchdog Lambda name (covers GitHub-cron auto-deploy misses)"
  value       = aws_lambda_function.deploy_watchdog.function_name
}

output "binary_sha_stale_alarm_name" {
  description = "B9 deploy provenance: CloudWatch alarm firing when the deployed binary's git SHA lags main HEAD across a full 24h of watchdog samples"
  value       = aws_cloudwatch_metric_alarm.binary_sha_stale.alarm_name
}
