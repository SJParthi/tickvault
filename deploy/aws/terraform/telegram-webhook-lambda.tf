# Telegram webhook Lambda — Z+ L1 DETECT layer.
#
# Subscribes to the tv_alerts SNS topic. Every CloudWatch alarm fire
# (and every direct `aws sns publish` from deploy-aws etc.) lands in
# this Lambda, which fetches the bot token + chat ID from SSM and
# POSTs to the Telegram Bot API.
#
# Charter authority: operator-charter-forever.md §C row "100% alerting"
# + §F "Severity::Critical → Telegram". Without this Lambda the
# operator gets email-only delivery, which is unacceptable during
# market hours (09:15–15:30 IST).
#
# Cost: invocations well inside AWS Lambda free tier (1M/mo free),
# Telegram bot API is free, SSM GetParameter is free for SecureString
# at our volume. Net: ₹0/mo additional spend.
#
# Operator prerequisites BEFORE this is useful:
#   1. Create a Telegram bot via @BotFather → get the bot token.
#   2. Start a chat with the bot, /start → grab the numeric chat ID
#      via https://api.telegram.org/bot<TOKEN>/getUpdates
#   3. Push both to SSM via scripts/aws-seed-ssm-parameters.sh:
#        /tickvault/prod/telegram/bot-token   (SecureString)
#        /tickvault/prod/telegram/chat-id     (SecureString)
#
# When credentials are missing the Lambda raises an exception, SNS
# retries per its default policy, and a CloudWatch metric records the
# failure. There is no silent drop.

# ---------------------------------------------------------------------------
# IAM role — minimum permissions (SSM GetParameter + KMS Decrypt + logs)
# ---------------------------------------------------------------------------

data "aws_iam_policy_document" "telegram_webhook_assume" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

data "aws_iam_policy_document" "telegram_webhook_permissions" {
  # Read the two SecureString secrets out of SSM.
  statement {
    sid     = "SsmReadTelegramSecrets"
    effect  = "Allow"
    actions = ["ssm:GetParameter"]
    resources = [
      "arn:aws:ssm:${var.aws_region}:*:parameter${var.telegram_bot_token_ssm_param}",
      "arn:aws:ssm:${var.aws_region}:*:parameter${var.telegram_chat_id_ssm_param}",
    ]
  }

  # SecureString uses the AWS-managed KMS key by default; this allows
  # the Lambda to call Decrypt against it.
  statement {
    sid       = "KmsDecryptSsmSecureString"
    effect    = "Allow"
    actions   = ["kms:Decrypt"]
    resources = ["*"]
    condition {
      test     = "StringEquals"
      variable = "kms:ViaService"
      values   = ["ssm.${var.aws_region}.amazonaws.com"]
    }
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

resource "aws_iam_role" "telegram_webhook" {
  name               = "tv-${var.environment}-telegram-webhook-lambda"
  assume_role_policy = data.aws_iam_policy_document.telegram_webhook_assume.json
}

resource "aws_iam_role_policy" "telegram_webhook" {
  name   = "tv-${var.environment}-telegram-webhook-policy"
  role   = aws_iam_role.telegram_webhook.id
  policy = data.aws_iam_policy_document.telegram_webhook_permissions.json
}

# ---------------------------------------------------------------------------
# Lambda source — packaged from deploy/aws/lambda/telegram-webhook/
# ---------------------------------------------------------------------------

data "archive_file" "telegram_webhook" {
  type        = "zip"
  source_dir  = "${path.module}/../lambda/telegram-webhook"
  output_path = "${path.module}/.telegram-webhook.zip"
  excludes    = ["README.md", "__pycache__", "*.pyc"]
}

resource "aws_lambda_function" "telegram_webhook" {
  function_name    = "tv-${var.environment}-telegram-webhook"
  role             = aws_iam_role.telegram_webhook.arn
  handler          = "handler.lambda_handler"
  runtime          = "python3.12"
  timeout          = 15
  memory_size      = 128
  filename         = data.archive_file.telegram_webhook.output_path
  source_code_hash = data.archive_file.telegram_webhook.output_base64sha256

  environment {
    variables = {
      TELEGRAM_BOT_TOKEN_SSM_PARAM = var.telegram_bot_token_ssm_param
      TELEGRAM_CHAT_ID_SSM_PARAM   = var.telegram_chat_id_ssm_param
      LOG_LEVEL                    = "INFO"
    }
  }

  tags = {
    Name    = "tv-${var.environment}-telegram-webhook"
    Project = "tickvault"
    Layer   = "L1-DETECT"
  }
}

# ---------------------------------------------------------------------------
# CloudWatch Log Group — explicit so we control retention + delete on destroy
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_log_group" "telegram_webhook" {
  name              = "/aws/lambda/tv-${var.environment}-telegram-webhook"
  retention_in_days = 14
  tags = {
    Project = "tickvault"
    Layer   = "L1-DETECT"
  }
}

# ---------------------------------------------------------------------------
# SNS subscription — fan tv_alerts into the Lambda
# ---------------------------------------------------------------------------

resource "aws_lambda_permission" "telegram_webhook_sns" {
  statement_id  = "AllowExecutionFromSnsTvAlerts"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.telegram_webhook.function_name
  principal     = "sns.amazonaws.com"
  source_arn    = aws_sns_topic.tv_alerts.arn
}

resource "aws_sns_topic_subscription" "telegram_webhook" {
  topic_arn = aws_sns_topic.tv_alerts.arn
  protocol  = "lambda"
  endpoint  = aws_lambda_function.telegram_webhook.arn
}

# ---------------------------------------------------------------------------
# Alarm on Lambda errors — so a broken webhook does NOT go silent.
# If the Lambda itself errors > 0 times in a 5-min window, the operator
# learns about it through the email subscription (which IS already wired).
# This is the L1 DETECT layer's self-defense per charter §C.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "telegram_webhook_errors" {
  alarm_name          = "tv-${var.environment}-telegram-webhook-errors"
  alarm_description   = "Telegram webhook Lambda is failing — Telegram alerts may be silently dropped. Check CloudWatch logs at /aws/lambda/tv-${var.environment}-telegram-webhook."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"

  dimensions = {
    FunctionName = aws_lambda_function.telegram_webhook.function_name
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# Outputs
# ---------------------------------------------------------------------------

output "telegram_webhook_lambda_name" {
  description = "Name of the Telegram webhook Lambda function."
  value       = aws_lambda_function.telegram_webhook.function_name
}

output "telegram_webhook_lambda_arn" {
  description = "ARN of the Telegram webhook Lambda function."
  value       = aws_lambda_function.telegram_webhook.arn
}
