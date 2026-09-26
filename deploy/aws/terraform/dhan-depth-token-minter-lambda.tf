# =============================================================================
# Dhan DEPTH-account access-token minter — one minter per account
# (groww-shared-token-minter-2026-07-02.md §10.9, operator 2026-09-26)
# =============================================================================
# The operator is opening a SECOND Dhan account, in the operator's own name,
# used ONLY for extra depth-20 / depth-200 sockets (websocket-connection-scope-
# lock.md § "2026-09-26 — A SECOND DHAN ACCOUNT"). Dhan permits ONE active token
# per ACCOUNT, so this account needs exactly one minter of its own — this one.
#
# SAME CODE, DIFFERENT CONFIGURATION: this function runs the very same binary as
# dhan-token-minter-lambda.tf (the same zip). The only difference is
# SSM_SERVICE = "dhan-depth", which the Rust module checks against a closed list
# of two segments. There is no second implementation of the mint.
#
# ISOLATION: this role reads ONLY /tickvault/<env>/dhan-depth/{client-id,
# client-secret,totp-secret} and writes ONLY /tickvault/<env>/dhan-depth/
# access-token. It cannot touch the primary account's /dhan/ parameters, and
# the primary minter's role cannot touch these.
#
# SHIPS OFF: var.dhan_depth_account_enabled (default false) keeps the schedule
# DISABLED and the not-invoked alarm absent until the account exists and its
# parameters are seeded. The function, role and errors alarm exist from day one
# so enabling is a one-line flag flip with nothing else to deploy.
# =============================================================================

resource "aws_iam_role" "dhan_depth_token_minter" {
  name = "tv-${var.environment}-dhan-depth-token-minter-lambda"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "dhan_depth_token_minter" {
  name = "tv-${var.environment}-dhan-depth-token-minter-policy"
  role = aws_iam_role.dhan_depth_token_minter.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # READ the depth account's three credential parameters — enumerated,
        # never a wildcard.
        Effect = "Allow"
        Action = ["ssm:GetParameter"]
        Resource = [
          "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan-depth/client-id",
          "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan-depth/client-secret",
          "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan-depth/totp-secret",
        ]
      },
      {
        # WRITE exactly one parameter — the depth account's token.
        Effect   = "Allow"
        Action   = ["ssm:PutParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/dhan-depth/access-token"
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

resource "aws_lambda_function" "dhan_depth_token_minter" {
  function_name = "tv-${var.environment}-dhan-depth-token-minter"
  role          = aws_iam_role.dhan_depth_token_minter.arn
  handler       = "bootstrap"
  runtime       = "provided.al2023"
  architectures = ["arm64"]
  # Identical to the primary minter (§10.8): 12 + 20 + 30 + 20 = 82 s worst case.
  timeout          = 120
  memory_size      = 128
  filename         = "${path.module}/.lambda-zips/dhan-token-minter.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))

  environment {
    variables = {
      TV_ENVIRONMENT     = var.environment
      DHAN_AUTH_BASE_URL = "https://auth.dhan.co"
      # The ONLY difference from the primary minter: which account's
      # parameters it reads and writes.
      SSM_SERVICE = "dhan-depth"
      LOG_LEVEL   = "INFO"
    }
  }

  tags = {
    Name    = "tv-${var.environment}-dhan-depth-token-minter"
    Project = "tickvault"
    Layer   = "L4-PREVENT"
  }
}

resource "aws_cloudwatch_log_group" "dhan_depth_token_minter" {
  name              = "/aws/lambda/tv-${var.environment}-dhan-depth-token-minter"
  retention_in_days = 30
  tags = {
    Project = "tickvault"
    Layer   = "L4-PREVENT"
  }
}

# Same slot as the primary minter: 06:05 IST, EVERY day. Created DISABLED until
# the account exists (§10.9) — enabling it early would fail every morning on
# missing parameters.
resource "aws_cloudwatch_event_rule" "dhan_depth_token_minter" {
  name                = "tv-${var.environment}-dhan-depth-token-minter"
  description         = "06:05 IST daily Dhan DEPTH-account access-token mint (groww-shared-token-minter-2026-07-02.md §10.9); disabled until the account exists"
  schedule_expression = "cron(35 0 * * ? *)"
  state               = var.dhan_depth_account_enabled ? "ENABLED" : "DISABLED"
}

resource "aws_cloudwatch_event_target" "dhan_depth_token_minter" {
  rule      = aws_cloudwatch_event_rule.dhan_depth_token_minter.name
  target_id = "tv-dhan-depth-token-minter"
  arn       = aws_lambda_function.dhan_depth_token_minter.arn
  input     = jsonencode({ mode = "scheduled_mint" })
}

resource "aws_lambda_permission" "dhan_depth_token_minter" {
  statement_id  = "AllowExecutionFromEventBridgeDhanDepthTokenMinter"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.dhan_depth_token_minter.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.dhan_depth_token_minter.arn
}

# A failed mint pages. With the schedule disabled nothing invokes the function,
# so this alarm stays quiet (notBreaching) until the account is switched on.
resource "aws_cloudwatch_metric_alarm" "dhan_depth_token_minter_errors" {
  alarm_name          = "tv-${var.environment}-dhan-depth-token-minter-errors"
  alarm_description   = "The daily Dhan DEPTH-account token mint FAILED. The extra depth sockets on the second account stay down until the next successful mint; the primary account's feed, depth and orders are unaffected."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 86400
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.dhan_depth_token_minter.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = []
}

# The mint NOT RUNNING. Gated on the same switch as the schedule (§10.9): while
# the schedule is disabled there are no invocations by design, and a breaching
# alarm would page every day.
resource "aws_cloudwatch_metric_alarm" "dhan_depth_token_minter_not_invoked" {
  count               = var.dhan_depth_account_enabled ? 1 : 0
  alarm_name          = "tv-${var.environment}-dhan-depth-token-minter-not-invoked"
  alarm_description   = "The daily Dhan DEPTH-account token mint did NOT RUN in the last 24h - the EventBridge schedule was dropped or disabled. The Errors alarm cannot see this (no invocation = no error)."
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
    FunctionName = aws_lambda_function.dhan_depth_token_minter.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = []
}
