# Operator-control Lambda — action backend for the single-page operator console
# (start / stop / restart-app / stop-app / restart-questdb / status).
#
# WHY: the operator wants ONE place (console URL + Telegram) to view AND control
# the box, without the AWS console or GitHub UI. Grafana = view; this = control.
# A console button POSTs an authenticated request to this Lambda's Function URL.
#
# SAFETY:
#   * Feature-flagged OFF (var.enable_operator_control_lambda, default false) —
#     creates NOTHING until the operator opts in + applies. Zero risk today.
#   * IAM scoped to EXACTLY this instance: ec2 Start/Stop/Reboot on the one ARN,
#     ssm:SendCommand on the one instance + AWS-RunShellScript only. Nothing else.
#   * Function URL auth = NONE at AWS, but the handler requires
#     `Authorization: Bearer <secret>` (constant-time compare) where the secret
#     is read at runtime from the SSM SecureString below — NEVER in env/state.
#   * Destructive actions are market-hours-guarded in the handler (force to override).
#   * `deploy` is intentionally NOT here (needs a GitHub PAT we don't store) —
#     deploy stays auto-on-merge; the console links to Actions for it.
#
# Cost: invocations inside Lambda free tier (1M/mo); SSM RunCommand free.

variable "enable_operator_control_lambda" {
  description = "Deploy the operator portal Lambda + Function URL. Requires SSM SecureString /tickvault/<env>/operator/control-secret (+ /operator/github-token for the GitHub tab)."
  type        = bool
  default     = false
}

variable "operator_github_repo" {
  description = "owner/repo the operator portal's GitHub tab acts on (view PRs, squash-merge, trigger deploy)."
  type        = string
  default     = "SJParthi/tickvault"
}

data "aws_iam_policy_document" "operator_control_assume" {
  count = var.enable_operator_control_lambda ? 1 : 0
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

data "aws_iam_policy_document" "operator_control_permissions" {
  count = var.enable_operator_control_lambda ? 1 : 0

  statement {
    sid       = "Ec2ControlTvAppOnly"
    effect    = "Allow"
    actions   = ["ec2:StartInstances", "ec2:StopInstances", "ec2:RebootInstances"]
    resources = ["arn:aws:ec2:${var.aws_region}:*:instance/${aws_instance.tv_app.id}"]
  }

  statement {
    sid       = "Ec2Describe"
    effect    = "Allow"
    actions   = ["ec2:DescribeInstances"]
    resources = ["*"]
  }

  # ssm:SendCommand fires the shell; ssm:GetCommandInvocation reads the output
  # back for the synchronous "view" snapshot (instance/app/tick/candle status).
  # SendCommand is scoped to the one instance + AWS-RunShellScript only.
  statement {
    sid     = "SsmSendCommand"
    effect  = "Allow"
    actions = ["ssm:SendCommand"]
    resources = [
      "arn:aws:ec2:${var.aws_region}:*:instance/${aws_instance.tv_app.id}",
      "arn:aws:ssm:${var.aws_region}::document/AWS-RunShellScript",
    ]
  }

  # GetCommandInvocation has no resource-level scoping (AWS requires "*"); it is
  # a read of command output keyed by the command id this Lambda itself created.
  statement {
    sid       = "SsmReadCommandOutput"
    effect    = "Allow"
    actions   = ["ssm:GetCommandInvocation"]
    resources = ["*"]
  }

  statement {
    sid       = "ReadControlSecret"
    effect    = "Allow"
    actions   = ["ssm:GetParameter"]
    resources = ["arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/operator/control-secret"]
  }

  # Fine-grained GitHub PAT (scoped to the one repo) for the GitHub tab —
  # view PRs, squash-merge, trigger deploy. Read of the SecureString only.
  statement {
    sid       = "ReadGithubToken"
    effect    = "Allow"
    actions   = ["ssm:GetParameter"]
    resources = ["arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/operator/github-token"]
  }

  # AWS tab: read-only CloudWatch alarm states + month-to-date cost. No
  # resource-level scoping is available for these read APIs (AWS requires "*").
  statement {
    sid       = "ReadOnlyObservability"
    effect    = "Allow"
    actions   = ["cloudwatch:DescribeAlarms", "ce:GetCostAndUsage"]
    resources = ["*"]
  }

  # Review finding 7: pin logs to THIS account + this Lambda's log group only.
  statement {
    sid    = "LambdaLogs"
    effect = "Allow"
    actions = [
      "logs:CreateLogGroup",
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    resources = [
      "arn:aws:logs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda/tv-${var.environment}-operator-control:*",
    ]
  }
}

resource "aws_iam_role" "operator_control" {
  count              = var.enable_operator_control_lambda ? 1 : 0
  name               = "tv-${var.environment}-operator-control-lambda"
  assume_role_policy = data.aws_iam_policy_document.operator_control_assume[0].json
}

resource "aws_iam_role_policy" "operator_control" {
  count  = var.enable_operator_control_lambda ? 1 : 0
  name   = "tv-${var.environment}-operator-control-permissions"
  role   = aws_iam_role.operator_control[0].id
  policy = data.aws_iam_policy_document.operator_control_permissions[0].json
}

data "archive_file" "operator_control" {
  count       = var.enable_operator_control_lambda ? 1 : 0
  type        = "zip"
  source_dir  = "${path.module}/../lambda/operator-control"
  output_path = "${path.module}/.build/operator-control.zip"
  excludes    = ["test_handler.py", "README.md"] # ship handler.py only — not test/docs
}

resource "aws_lambda_function" "operator_control" {
  count            = var.enable_operator_control_lambda ? 1 : 0
  function_name    = "tv-${var.environment}-operator-control"
  role             = aws_iam_role.operator_control[0].arn
  runtime          = "python3.12"
  handler          = "handler.lambda_handler"
  filename         = data.archive_file.operator_control[0].output_path
  source_code_hash = data.archive_file.operator_control[0].output_base64sha256
  timeout          = 30
  memory_size      = 128

  environment {
    variables = {
      # NOTE: this is the PARAMETER NAME, not the secret. The handler reads the
      # actual secret from SSM (decrypted) at cold start — nothing sensitive
      # lands in env or Terraform state. AWS_REGION is auto-provided by Lambda.
      TV_INSTANCE_ID                = aws_instance.tv_app.id
      OPERATOR_CONTROL_SECRET_PARAM = "/tickvault/${var.environment}/operator/control-secret"
      OPERATOR_GITHUB_TOKEN_PARAM   = "/tickvault/${var.environment}/operator/github-token"
      GH_REPO                       = var.operator_github_repo
      GH_DEPLOY_WORKFLOW            = "deploy-aws.yml"
    }
  }
}

resource "aws_lambda_function_url" "operator_control" {
  count              = var.enable_operator_control_lambda ? 1 : 0
  function_name      = aws_lambda_function.operator_control[0].function_name
  authorization_type = "NONE" # bearer-secret enforced inside the handler

  cors {
    allow_origins = ["*"]
    allow_methods = ["GET", "POST"] # GET serves the console page; POST runs actions
    allow_headers = ["authorization", "content-type"]
    max_age       = 300
  }
}

# Per-Lambda error alarm → existing tv_alerts SNS (matches the killswitch pattern).
resource "aws_cloudwatch_metric_alarm" "operator_control_errors" {
  count               = var.enable_operator_control_lambda ? 1 : 0
  alarm_name          = "tv-${var.environment}-operator-control-errors"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  alarm_description   = "operator-control Lambda erroring — control buttons may be failing"
  dimensions          = { FunctionName = aws_lambda_function.operator_control[0].function_name }
  treat_missing_data  = "notBreaching"
}

output "operator_control_function_url" {
  value       = var.enable_operator_control_lambda ? aws_lambda_function_url.operator_control[0].function_url : null
  description = "Open this URL in a browser = the operator console (GET serves the page). Buttons POST with header 'Authorization: Bearer <secret>' + body {\"action\":\"view|start|stop|reboot|restart-app|stop-app|restart-questdb\"}"
}
