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
        # Curfew guard keep-alive override (2026-07-03 operator directive):
        # the hourly curfew_check reads /tickvault/<env>/keep-alive-until to
        # decide whether an out-of-hours running box is deliberate.
        # NSE-holiday intentional-stop marker (2026-07-07 round-3 review
        # fix): the 08:45 check reads /tickvault/<env>/holiday-stop-date
        # (stamped by deploy/aws/holiday-gate.sh before its holiday
        # self-stop) so it never "heals" an intentional holiday stop — that
        # self-start both false-paged every holiday and fed the restart war
        # racing the 09:20 IST alarm-gate sample. Read-only, scoped to
        # exactly these two parameters.
        Effect = "Allow"
        Action = ["ssm:GetParameter"]
        Resource = [
          "arn:aws:ssm:${var.aws_region}:${data.aws_caller_identity.current.account_id}:parameter/tickvault/${var.environment}/keep-alive-until",
          "arn:aws:ssm:${var.aws_region}:${data.aws_caller_identity.current.account_id}:parameter/tickvault/${var.environment}/holiday-stop-date",
        ]
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
# Lambda source — Rust bin (rust-only phase 2b-2 wave 2, 2026-07-18).
#
# The Python handler (deploy/aws/lambda/start-watchdog/handler.py) was
# PORTED to Rust — crates/aws-lambdas/src/start_watchdog.rs (lib logic;
# all 31 python tests ported 1:1 to Rust unit tests) +
# src/bin/start_watchdog.rs (thin bootstrap bin). Behavior parity: same
# mode dispatch (ping/check/stop_check/curfew_check), same SNS
# Subject/Message strings, same IST curfew math (08:00-17:00 Mon-Fri),
# same keep-alive/holiday-stop SSM semantics, same 45-min launch grace.
# The zip is built in CI by the build-lambdas job (terraform-apply.yml)
# into ${path.module}/.lambda-zips/ before plan/apply; source_code_hash is
# a digest of the Rust SOURCE (Rust builds are not bit-reproducible, so
# hashing the zip would churn every build with zero source change).
# ---------------------------------------------------------------------------

resource "aws_lambda_function" "start_watchdog" {
  function_name    = "tv-${var.environment}-start-watchdog"
  role             = aws_iam_role.start_watchdog.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  timeout          = 30
  memory_size      = 128
  filename         = "${path.module}/.lambda-zips/start-watchdog.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))

  environment {
    variables = {
      EC2_INSTANCE_ID  = aws_instance.tv_app.id
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
      # App port for the Feed Control page link in the 08:30 IST start ping
      # (config/base.toml [api] port). The ping reads the instance's live
      # public IP via the existing ec2:DescribeInstances grant — no new IAM.
      DASHBOARD_PORT = "3001"
      LOG_LEVEL      = "INFO"
      # Curfew keep-alive override param (ISO timestamp; operator/portal sets
      # it to run the box out of hours without the curfew guard stopping it).
      KEEP_ALIVE_PARAM = "/tickvault/${var.environment}/keep-alive-until"
      # NSE-holiday intentional-stop marker (2026-07-07 round-3 review fix):
      # holiday-gate.sh stamps today's IST date here before its self-stop;
      # the 08:45 check skips the self-start + Critical page when it matches.
      HOLIDAY_STOP_PARAM = "/tickvault/${var.environment}/holiday-stop-date"
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
  # RESUME 2026-07-06 per operator GO (moved up from Thu Jul 9): explicit state
  # so terraform re-enables this rule. The Jul 4 pause set state = "DISABLED";
  # the #1404 revert REMOVED the attribute, and with `state` absent the AWS
  # provider stops managing rule state - the rule stayed DISABLED on AWS.
  state = "ENABLED"
}

resource "aws_cloudwatch_event_rule" "start_watchdog_check" {
  name                = "tv-${var.environment}-start-watchdog-check"
  description         = "08:45 IST (Mon-Fri) verify the box actually started; page if not"
  schedule_expression = "cron(15 3 ? * MON-FRI *)"
  # RESUME 2026-07-06 per operator GO (moved up from Thu Jul 9): explicit state
  # so terraform re-enables this rule. The Jul 4 pause set state = "DISABLED";
  # the #1404 revert REMOVED the attribute, and with `state` absent the AWS
  # provider stops managing rule state - the rule stayed DISABLED on AWS.
  state = "ENABLED"
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

# Hourly out-of-hours CURFEW guard — operator directive 2026-07-03 ("manually
# I started this instance right out of the expected market hours … by mistake
# if I don't stop manually means it should be auto triggered … manual human
# error is not acceptable"). Fires every hour; the HANDLER gates itself so it
# NEVER acts inside 08:00-17:00 IST Mon-Fri (the normal 08:30/16:30 schedule +
# 16:45 stop_check own that window, unchanged). Outside the window a running
# box with no keep-alive override (SSM /tickvault/<env>/keep-alive-until) and
# past the 45-min launch grace is stopped + Telegram-paged. Cost: 24
# invokes/day — free tier.
resource "aws_cloudwatch_event_rule" "start_watchdog_curfew_check" {
  name                = "tv-${var.environment}-start-watchdog-curfew-check"
  description         = "Hourly out-of-hours curfew: stop a forgotten manually-started box (keep-alive override honored)"
  schedule_expression = "cron(5 * * * ? *)"
}

resource "aws_cloudwatch_event_target" "start_watchdog_curfew_check" {
  rule      = aws_cloudwatch_event_rule.start_watchdog_curfew_check.name
  target_id = "start-watchdog-curfew-check"
  arn       = aws_lambda_function.start_watchdog.arn
  input     = jsonencode({ mode = "curfew_check" })
}

resource "aws_lambda_permission" "start_watchdog_curfew_check" {
  statement_id  = "AllowExecutionFromEventBridgeCurfewCheck"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.start_watchdog.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.start_watchdog_curfew_check.arn
}

output "start_watchdog_function_name" {
  description = "Instance start-watchdog Lambda name"
  value       = aws_lambda_function.start_watchdog.function_name
}
