# Phase 8.2 of .claude/plans/active-plan.md — claude-triage Lambda.
#
# Dispatches CloudWatch alarm events from the tv_alerts SNS topic to
# a Claude Code session running on the EC2 instance, via SSM
# RunCommand. The long-running claude CLI then applies the triage
# rules YAML and either auto-fixes, silences, or escalates.
#
# Gated behind `var.enable_claude_triage_lambda` — defaults to false so
# the resource block creates nothing until the operator opts in. This
# keeps the stack free of unused Lambda + IAM role during early
# deployment.
#
# Cost: invocations well inside AWS Lambda free tier (1M/mo free),
# SSM RunCommand is free, CloudWatch Logs inside 5GB free tier.

variable "enable_claude_triage_lambda" {
  description = "Deploy the claude-triage Lambda + SNS subscription. Requires a `claude-triage` tmux session pre-created on the EC2."
  type        = bool
  default     = false
}

# ---------------------------------------------------------------------------
# IAM role for the Lambda — scoped permissions only
# ---------------------------------------------------------------------------

data "aws_iam_policy_document" "claude_triage_assume" {
  count = var.enable_claude_triage_lambda ? 1 : 0

  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

data "aws_iam_policy_document" "claude_triage_permissions" {
  count = var.enable_claude_triage_lambda ? 1 : 0

  # Send commands to the EC2 instance via SSM.
  statement {
    sid    = "SsmRunCommand"
    effect = "Allow"
    actions = [
      "ssm:SendCommand",
      "ssm:GetCommandInvocation",
      "ssm:ListCommandInvocations",
    ]
    resources = [
      "arn:aws:ec2:${var.aws_region}:*:instance/${aws_instance.tv.id}",
      "arn:aws:ssm:${var.aws_region}::document/AWS-RunShellScript",
    ]
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

resource "aws_iam_role" "claude_triage" {
  count              = var.enable_claude_triage_lambda ? 1 : 0
  name               = "tv-${var.environment}-claude-triage-lambda"
  assume_role_policy = data.aws_iam_policy_document.claude_triage_assume[0].json
}

resource "aws_iam_role_policy" "claude_triage" {
  count  = var.enable_claude_triage_lambda ? 1 : 0
  name   = "tv-${var.environment}-claude-triage-policy"
  role   = aws_iam_role.claude_triage[0].id
  policy = data.aws_iam_policy_document.claude_triage_permissions[0].json
}

# ---------------------------------------------------------------------------
# Lambda source — packaged from deploy/aws/lambda/claude-triage/
# ---------------------------------------------------------------------------

data "archive_file" "claude_triage" {
  count       = var.enable_claude_triage_lambda ? 1 : 0
  type        = "zip"
  source_dir  = "${path.module}/../lambda/claude-triage"
  output_path = "${path.module}/.claude-triage.zip"
  excludes    = ["README.md"]
}

resource "aws_lambda_function" "claude_triage" {
  count            = var.enable_claude_triage_lambda ? 1 : 0
  function_name    = "tv-${var.environment}-claude-triage"
  role             = aws_iam_role.claude_triage[0].arn
  handler          = "handler.lambda_handler"
  runtime          = "python3.12"
  timeout          = 60
  memory_size      = 128
  filename         = data.archive_file.claude_triage[0].output_path
  source_code_hash = data.archive_file.claude_triage[0].output_base64sha256

  environment {
    variables = {
      EC2_INSTANCE_ID        = aws_instance.tv.id
      TRIAGE_COMMAND_TIMEOUT = "180"
      CLAUDE_BINARY_PATH     = "/usr/local/bin/claude"
      TV_LOG_GROUP           = aws_cloudwatch_log_group.tv_app.name
    }
  }

  tags = {
    Name    = "tv-${var.environment}-claude-triage"
    Project = "tickvault"
    Phase   = "8.2"
  }
}

# ---------------------------------------------------------------------------
# SNS subscription — fan CloudWatch alarm fire-outs into the Lambda
# ---------------------------------------------------------------------------

resource "aws_lambda_permission" "claude_triage_sns" {
  count         = var.enable_claude_triage_lambda ? 1 : 0
  statement_id  = "AllowExecutionFromSNS"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.claude_triage[0].function_name
  principal     = "sns.amazonaws.com"
  source_arn    = aws_sns_topic.tv_alerts.arn
}

resource "aws_sns_topic_subscription" "claude_triage" {
  count     = var.enable_claude_triage_lambda ? 1 : 0
  topic_arn = aws_sns_topic.tv_alerts.arn
  protocol  = "lambda"
  endpoint  = aws_lambda_function.claude_triage[0].arn
}

# ---------------------------------------------------------------------------
# Output — useful for operator debugging
# ---------------------------------------------------------------------------

output "claude_triage_lambda_name" {
  description = "Name of the claude-triage Lambda function (null when disabled)"
  value = (
    var.enable_claude_triage_lambda
    ? aws_lambda_function.claude_triage[0].function_name
    : null
  )
}

output "claude_triage_lambda_arn" {
  description = "ARN of the claude-triage Lambda function (null when disabled)"
  value = (
    var.enable_claude_triage_lambda
    ? aws_lambda_function.claude_triage[0].arn
    : null
  )
}
