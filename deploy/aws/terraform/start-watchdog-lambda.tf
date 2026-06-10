# Instance start-watchdog Lambda — answers "who monitors the 08:30 start?".
#
# 2026-06-02 incident: the EventBridge -> SSM-Automation 08:30 start silently
# failed and NOTHING alerted the operator until he noticed by hand.
# 2026-06-10 REPEAT: same silent start failure; the operator's manual 08:43
# start beat the 08:45 check, so detection alone stayed silent and the broken
# start path went unflagged. The Lambda therefore now (a) SELF-HEALS — issues
# ec2:StartInstances itself when the box is down at 08:45 — and (b) flags a
# running-but-launched-LATE box so a masked auto-start failure still pages.
# aws-autopilot (GitHub Actions, every 15 min) also covers it, but depends on
# the GH runner firing. This Lambda is the AWS-native belt-and-suspenders: it
# runs IN AWS, so it detects AND heals even if GitHub Actions is down.
#
# Two EventBridge schedules invoke the same Lambda with a `mode` input:
#   * ping  @ 03:00 UTC = 08:30 IST (fires with daily_start) -> positive
#     "start triggered" Telegram.
#   * check @ 03:15 UTC = 08:45 IST -> ec2:DescribeInstances; if NOT running,
#     ec2:StartInstances (self-heal) + Severity::Critical Telegram. If running
#     but LaunchTime is after 03:05 UTC, a "started late" warning. Silent only
#     when the box came up on time.
#
# Cost: 2 invokes/weekday (~44/mo) — free tier (1M req/mo). Effectively ₹0.

# ---------------------------------------------------------------------------
# IAM — assume + least-privilege (describe EC2 + publish to tv_alerts + logs)
# ---------------------------------------------------------------------------

resource "aws_iam_role" "start_watchdog" {
  name = "tv-${var.environment}-start-watchdog-lambda"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "start_watchdog" {
  name = "tv-${var.environment}-start-watchdog-policy"
  role = aws_iam_role.start_watchdog.id
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
        # Self-heal (2026-06-10 incident, repeat of 2026-06-02): when the
        # 08:45 IST check finds the box not running, the watchdog issues
        # StartInstances itself instead of only paging. Same evening, the
        # 16:30 stop ALSO silently failed (box still running at 18:18 IST),
        # so the 16:45 stop_check gained StopInstances on the same terms.
        # Scoped to the one tv-app instance — this Lambda can touch
        # nothing else.
        Effect   = "Allow"
        Action   = ["ec2:StartInstances", "ec2:StopInstances"]
        Resource = "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:instance/${aws_instance.tv_app.id}"
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
# Lambda source — packaged from deploy/aws/lambda/start-watchdog/
# ---------------------------------------------------------------------------

data "archive_file" "start_watchdog" {
  type        = "zip"
  source_dir  = "${path.module}/../lambda/start-watchdog"
  output_path = "${path.module}/.start-watchdog.zip"
  excludes    = ["README.md", "test_handler.py", "__pycache__", "*.pyc"]
}

resource "aws_lambda_function" "start_watchdog" {
  function_name    = "tv-${var.environment}-start-watchdog"
  role             = aws_iam_role.start_watchdog.arn
  handler          = "handler.lambda_handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  filename         = data.archive_file.start_watchdog.output_path
  source_code_hash = data.archive_file.start_watchdog.output_base64sha256

  environment {
    variables = {
      EC2_INSTANCE_ID  = aws_instance.tv_app.id
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
      LOG_LEVEL        = "INFO"
    }
  }

  tags = {
    Name    = "tv-${var.environment}-start-watchdog"
    Project = "tickvault"
    Layer   = "L1-DETECT"
  }
}

resource "aws_cloudwatch_log_group" "start_watchdog" {
  name              = "/aws/lambda/tv-${var.environment}-start-watchdog"
  retention_in_days = 30
  tags = {
    Project = "tickvault"
    Layer   = "L1-DETECT"
  }
}

# ---------------------------------------------------------------------------
# EventBridge schedules — ping (08:30 IST) + check (08:45 IST), weekdays
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_event_rule" "start_watchdog_ping" {
  name                = "tv-${var.environment}-start-watchdog-ping"
  description         = "08:30 IST (Mon-Fri) positive 'instance start triggered' Telegram"
  schedule_expression = "cron(0 3 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_rule" "start_watchdog_check" {
  name                = "tv-${var.environment}-start-watchdog-check"
  description         = "08:45 IST (Mon-Fri) verify the box actually started; page if not"
  schedule_expression = "cron(15 3 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_target" "start_watchdog_ping" {
  rule      = aws_cloudwatch_event_rule.start_watchdog_ping.name
  target_id = "start-watchdog-ping"
  arn       = aws_lambda_function.start_watchdog.arn
  input     = jsonencode({ mode = "ping" })
}

resource "aws_cloudwatch_event_target" "start_watchdog_check" {
  rule      = aws_cloudwatch_event_rule.start_watchdog_check.name
  target_id = "start-watchdog-check"
  arn       = aws_lambda_function.start_watchdog.arn
  input     = jsonencode({ mode = "check" })
}

resource "aws_lambda_permission" "start_watchdog_ping" {
  statement_id  = "AllowExecutionFromEventBridgePing"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.start_watchdog.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.start_watchdog_ping.arn
}

resource "aws_lambda_permission" "start_watchdog_check" {
  statement_id  = "AllowExecutionFromEventBridgeCheck"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.start_watchdog.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.start_watchdog_check.arn
}

# 16:45 IST stop-check — added 2026-06-10 after BOTH schedule directions
# failed silently the same day (start at 08:30, stop at 16:30). Verifies the
# 16:30 daily_stop actually stopped the box; self-heals (stop) ONLY when the
# box has been running since before 16:30 — a manual evening start (launch
# after 16:30) is the operator's deliberate session and is never touched.
resource "aws_cloudwatch_event_rule" "start_watchdog_stop_check" {
  name                = "tv-${var.environment}-start-watchdog-stop-check"
  description         = "16:45 IST (Mon-Fri) verify the box actually stopped; self-heal + page if not"
  schedule_expression = "cron(15 11 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_target" "start_watchdog_stop_check" {
  rule      = aws_cloudwatch_event_rule.start_watchdog_stop_check.name
  target_id = "start-watchdog-stop-check"
  arn       = aws_lambda_function.start_watchdog.arn
  input     = jsonencode({ mode = "stop_check" })
}

resource "aws_lambda_permission" "start_watchdog_stop_check" {
  statement_id  = "AllowExecutionFromEventBridgeStopCheck"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.start_watchdog.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.start_watchdog_stop_check.arn
}

output "start_watchdog_function_name" {
  description = "Instance start-watchdog Lambda name"
  value       = aws_lambda_function.start_watchdog.function_name
}
