# =============================================================================
# Dhan access-token minter Lambda — the box-independent daily mint
# (operator directive 2026-08-08: "build the lambda")
# =============================================================================
# THE INCIDENT THIS CLOSES: Dhan permits exactly ONE active access token per
# account. Until now the ONLY thing that minted a Dhan token was the tickvault
# app on the prod EC2 box, which coupled token freshness to the box being UP.
#
# On 2026-08-06 the box stopped starting (ap-south-1a InsufficientInstanceCapacity
# — see daily-universe-scope-expansion-2026-05-27.md §0 Quote 12; Aug 5 = 0h,
# Aug 7 = 0h). With the box down nothing minted, so
# /tickvault/<env>/dhan/access-token went stale (last write 25 July) and every
# consumer reading it got HTTP 401 / DH-901 "invalid or expired" on every call.
#
# Groww never had this failure mode — its minter is a Lambda, which runs whether
# or not the box does. That asymmetry is exactly why the Groww parameter
# refreshed on schedule while the Dhan one sat frozen for two weeks. This is the
# Dhan mirror of that design.
#
# ONE MINTER PER BROKER is the invariant. This Lambda is the Dhan minter; every
# consumer READS. Contract: groww-shared-token-minter-2026-07-02.md §10.
#
# SEPARATION OF DUTIES: this Lambda mints and publishes. It does NOT start, stop
# or touch EC2, and it holds no EC2 permission at all.
# =============================================================================

# ---------------------------------------------------------------------------
# IAM — least privilege: read the 3 credential params, write the 1 token param
# ---------------------------------------------------------------------------

resource "aws_iam_role" "dhan_token_minter" {
  name = "tv-${var.environment}-dhan-token-minter-lambda"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "dhan_token_minter" {
  name = "tv-${var.environment}-dhan-token-minter-policy"
  role = aws_iam_role.dhan_token_minter.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # READ the three credential parameters — enumerated individually, NOT
        # a /dhan/* wildcard, so this role can never read the token it writes
        # back (it has no reason to) nor any future Dhan parameter.
        Effect = "Allow"
        Action = ["ssm:GetParameter"]
        Resource = [
          "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan/client-id",
          "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan/client-secret",
          "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan/totp-secret",
        ]
      },
      {
        # WRITE exactly one parameter — the shared token. Scoped to that single
        # ARN so a bug here can never overwrite the GROWW token (same secret
        # name, different service segment) or any other parameter.
        Effect   = "Allow"
        Action   = ["ssm:PutParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan/access-token"
      },
      {
        # The credential params are SecureString. They use the DEFAULT aws/ssm
        # key, whose key policy grants use to the account via SSM — no explicit
        # kms:Decrypt statement is needed here (contrast the instance role,
        # which needs one only for the Groww param's customer-managed CMK).
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
# Lambda — Rust bin, built by the build-lambdas job in terraform-apply.yml
# (cargo lambda build -p tickvault-aws-lambdas builds every [[bin]], so this
# one is picked up with no workflow change).
# ---------------------------------------------------------------------------

resource "aws_lambda_function" "dhan_token_minter" {
  function_name = "tv-${var.environment}-dhan-token-minter"
  role          = aws_iam_role.dhan_token_minter.arn
  handler       = "bootstrap"
  runtime       = "provided.al2023"
  architectures = ["arm64"]
  # 60s: the mint is one HTTP POST (bounded to 20s by MINT_TIMEOUT_SECS) plus
  # 3 concurrent SSM reads and 1 SSM write. Generous headroom for a cold start
  # without letting a hung call burn minutes.
  timeout          = 60
  memory_size      = 128
  filename         = "${path.module}/.lambda-zips/dhan-token-minter.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))

  environment {
    variables = {
      TV_ENVIRONMENT = var.environment
      # Terraform is the config source of truth for this host. The Rust module
      # deliberately carries NO in-code default (the house banned-pattern
      # scanner forbids hardcoded URLs in production source), so an absent
      # value fails the mint loudly instead of guessing a host.
      DHAN_AUTH_BASE_URL = "https://auth.dhan.co"
      LOG_LEVEL          = "INFO"
    }
  }

  tags = {
    Name    = "tv-${var.environment}-dhan-token-minter"
    Project = "tickvault"
    Layer   = "L4-PREVENT"
  }
}

resource "aws_cloudwatch_log_group" "dhan_token_minter" {
  name              = "/aws/lambda/tv-${var.environment}-dhan-token-minter"
  retention_in_days = 30
  tags = {
    Project = "tickvault"
    Layer   = "L4-PREVENT"
  }
}

# ---------------------------------------------------------------------------
# EventBridge schedule — 06:05 IST (00:35 UTC), EVERY day
#
# Mirrors the Groww minter's slot deliberately: Dhan's token day rolls over in
# the early morning, so minting at 06:05 IST gives a fresh token ~2.5 hours
# before the 08:30 IST box start and ~3 hours before market open.
#
# EVERY day, not MON-FRI: consumers (brutex backtests, our own REST legs) read
# this parameter on weekends too, and a Saturday with a Friday-expired token is
# the exact stale-parameter failure this Lambda exists to prevent.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_event_rule" "dhan_token_minter" {
  name                = "tv-${var.environment}-dhan-token-minter"
  description         = "06:05 IST daily Dhan access-token mint - box-independent (the 2026-08-06 capacity outage froze this parameter for two weeks; groww-shared-token-minter-2026-07-02.md §10)"
  schedule_expression = "cron(35 0 * * ? *)"
  # Explicit state so terraform manages it (the Jul-4 pause/#1404 incident
  # class — a rule WITHOUT the attribute is not state-managed by the provider).
  state = "ENABLED"
}

resource "aws_cloudwatch_event_target" "dhan_token_minter" {
  rule      = aws_cloudwatch_event_rule.dhan_token_minter.name
  target_id = "tv-dhan-token-minter"
  arn       = aws_lambda_function.dhan_token_minter.arn
  input     = jsonencode({ mode = "scheduled_mint" })
}

resource "aws_lambda_permission" "dhan_token_minter" {
  statement_id  = "AllowExecutionFromEventBridgeDhanTokenMinter"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.dhan_token_minter.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.dhan_token_minter.arn
}

# ---------------------------------------------------------------------------
# Watch the watchman — a silently-failing minter recreates the exact incident
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "dhan_token_minter_errors" {
  alarm_name        = "tv-${var.environment}-dhan-token-minter-errors"
  alarm_description = "The daily Dhan token mint FAILED. Every consumer of /tickvault/<env>/dhan/access-token is now serving a STALE token and will get DH-901 until the next successful mint. The handler deliberately returns Err on every failure path so a broken mint lands here instead of dying silently (Rule 11 - no false-OK)."
  # The Lambda runs once a day, so a 24h period is the correct window: a
  # shorter one is empty on 287 of every 288 five-minute windows and the alarm
  # would spend its life in INSUFFICIENT_DATA.
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 86400
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.dhan_token_minter.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  # NO ok_actions — the same reasoning as market-open-readiness: this Lambda
  # runs once a day, so a post-ALARM auto-OK only means the Errors datapoint
  # aged out of the lookback, never that anything was fixed. The
  # telegram-webhook Lambda forwards every OK as a green "recovered" page, so
  # symmetric ok_actions would emit a recurring false-recovery per failure.
  # Recovery signal = the next scheduled run succeeding.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# The mint NOT RUNNING AT ALL is a distinct failure from the mint FAILING.
#
# If EventBridge drops the schedule (the 2026-07-02 repo-wide scheduler-drop
# class) the Errors alarm above stays quiet forever — zero invocations means
# zero errors. This alarm keys on Invocations instead: fewer than 1 per day
# means nobody minted, which within 24h is a stale token.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "dhan_token_minter_not_invoked" {
  alarm_name          = "tv-${var.environment}-dhan-token-minter-not-invoked"
  alarm_description   = "The daily Dhan token mint did NOT RUN in the last 24h - the EventBridge schedule was dropped or disabled (the 2026-07-02 repo-wide scheduler-drop class). A mint that never runs is indistinguishable from a stale parameter to every consumer, and the Errors alarm cannot see it (no invocation = no error)."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Invocations"
  namespace           = "AWS/Lambda"
  period              = 86400
  statistic           = "Sum"
  threshold           = 1
  # breaching: a missing Invocations datapoint IS the condition being detected.
  treat_missing_data = "breaching"
  dimensions = {
    FunctionName = aws_lambda_function.dhan_token_minter.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = []
}
