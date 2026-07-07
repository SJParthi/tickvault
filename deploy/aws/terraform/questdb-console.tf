# B4 — QuestDB one-click console (2026-07-03).
# Re-trigger marker: bump to fire terraform-apply (re-zips both handlers). (b4-qdb-console-2026-07-03-r1)
#
# Browser → API-Gateway v2 HTTP API ($default, payload v2)
#         → FRONT Lambda (NON-VPC: device-key auth + session cookies +
#           server-side read-only SQL gate + path/method whitelist)
#         → lambda:InvokeFunction
#         → BACK Lambda (VPC-attached: dumb HTTP relay, ZERO secrets)
#         → http://<box_private_ip>:9000 (QuestDB console on the box).
#
# WHY TWO LAMBDAS: a VPC Lambda in this VPC has no internet/AWS-API path
# (single public subnet, no NAT, no VPC endpoints), so it cannot read the SSM
# device-key secret at runtime. Splitting keeps the secret handling identical
# to the existing operator-control posture (runtime SSM read, 60s cache,
# fail-closed, never in env vars / TF state) while the VPC hop stays
# secret-free.
#
# NETWORK POSTURE: the box SG gets exactly ONE new ingress rule — TCP 9000
# from the back-Lambda SG only (a dynamic inline block in main.tf keyed on
# var.enable_questdb_console; see the comment there for why inline-dynamic
# and not a separate rule resource). Port 9000 is NEVER public.
#
# All resources are count-gated on var.enable_questdb_console (variables.tf,
# default false; CI opts in via TF_VAR_enable_questdb_console) — rollback is
# a flag flip.

# ─────────────────────────────────────────────────────────────────────────────
# Security group for the VPC back Lambda: egress-only (it only ever dials the
# box). The matching 9000 ingress on aws_security_group.tv_app lives in
# main.tf as a dynamic inline block sourced from THIS SG's id.
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_security_group" "qdb_console_lambda" {
  count       = var.enable_questdb_console ? 1 : 0
  name        = "tv-${var.environment}-qdb-console-lambda"
  description = "QuestDB console proxy Lambda (B4): egress-only; box SG admits 9000 from this SG"
  vpc_id      = aws_vpc.dlt.id

  # Least-privilege egress: TCP 9000 into the VPC CIDR ONLY. We deliberately
  # scope to the VPC CIDR (aws_vpc.dlt.cidr_block, a static plan-time value)
  # rather than the box private IP (aws_instance.tv_app.private_ip): the box
  # is the sole :9000 listener in the /16, so port-locking to 9000 is the real
  # control, while referencing the instance here would drag the instance — and
  # therefore aws_security_group.tv_app, whose SG-to-SG ingress references THIS
  # SG — into a dependency cycle (qdb_console_lambda → instance → tv_app SG →
  # qdb_console_lambda). VPC-CIDR egress depends only on the VPC, breaking the
  # cycle while keeping every rule inline (no inline/external drift) and the
  # box's 9000 ingress SG-to-SG only (never public). The relay dials a raw IP,
  # so it needs no DNS/other egress.
  egress {
    description = "QuestDB :9000, VPC CIDR only (box is the sole :9000 listener)"
    from_port   = 9000
    to_port     = 9000
    protocol    = "tcp"
    cidr_blocks = [aws_vpc.dlt.cidr_block]
  }

  tags = {
    Name = "tv-${var.environment}-qdb-console-lambda-sg"
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# Packaging (mirrors the operator-control archive_file pattern).
# ─────────────────────────────────────────────────────────────────────────────
data "archive_file" "questdb_console_front" {
  count       = var.enable_questdb_console ? 1 : 0
  type        = "zip"
  source_dir  = "${path.module}/../lambda/questdb-console-front"
  output_path = "${path.module}/.build/questdb-console-front.zip"
  excludes    = ["test_handler.py", "README.md"] # ship handler.py only — not test/docs
}

data "archive_file" "questdb_console_proxy" {
  count       = var.enable_questdb_console ? 1 : 0
  type        = "zip"
  source_dir  = "${path.module}/../lambda/questdb-console-proxy"
  output_path = "${path.module}/.build/questdb-console-proxy.zip"
  excludes    = ["test_handler.py", "README.md"]
}

# ─────────────────────────────────────────────────────────────────────────────
# BACK Lambda (VPC): dumb relay to QuestDB on the box private IP. Zero secrets,
# zero AWS SDK calls at runtime — its IAM is ONLY the VPC-ENI + logs managed
# policy AWS requires for VPC attachment.
# ─────────────────────────────────────────────────────────────────────────────
data "aws_iam_policy_document" "questdb_console_assume" {
  count = var.enable_questdb_console ? 1 : 0
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "questdb_console_proxy" {
  count              = var.enable_questdb_console ? 1 : 0
  name               = "tv-${var.environment}-qdb-console-proxy-lambda"
  assume_role_policy = data.aws_iam_policy_document.questdb_console_assume[0].json
}

# ENI create/describe/delete + CloudWatch logs — required for vpc_config.
resource "aws_iam_role_policy_attachment" "questdb_console_proxy_vpc" {
  count      = var.enable_questdb_console ? 1 : 0
  role       = aws_iam_role.questdb_console_proxy[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole"
}

resource "aws_lambda_function" "questdb_console_proxy" {
  count            = var.enable_questdb_console ? 1 : 0
  function_name    = "tv-${var.environment}-questdb-console-proxy"
  role             = aws_iam_role.questdb_console_proxy[0].arn
  runtime          = "python3.12"
  handler          = "handler.lambda_handler"
  filename         = data.archive_file.questdb_console_proxy[0].output_path
  source_code_hash = data.archive_file.questdb_console_proxy[0].output_base64sha256
  timeout          = 26 # aggregate backstop: the handler's 12s _TIMEOUT_SECS is PER socket op (connect + EACH recv — _read_capped can issue up to ~16 sequential recvs), so a dribbling upstream is bounded by THIS Lambda timeout, not by 12s; < the front's 29s
  memory_size      = 256

  vpc_config {
    subnet_ids         = [aws_subnet.public.id] # the VPC's only subnet
    security_group_ids = [aws_security_group.qdb_console_lambda[0].id]
  }

  environment {
    variables = {
      # The box's PRIVATE IP inside the VPC — never hardcoded, never public.
      QDB_BASE = "http://${aws_instance.tv_app.private_ip}:9000"
    }
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# FRONT Lambda (non-VPC): auth + read-only SQL gate + relay. IAM = logs +
# exactly two grants: read the ONE control-secret param, invoke the ONE back
# Lambda.
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_iam_role" "questdb_console_front" {
  count              = var.enable_questdb_console ? 1 : 0
  name               = "tv-${var.environment}-qdb-console-front-lambda"
  assume_role_policy = data.aws_iam_policy_document.questdb_console_assume[0].json
}

resource "aws_iam_role_policy_attachment" "questdb_console_front_logs" {
  count      = var.enable_questdb_console ? 1 : 0
  role       = aws_iam_role.questdb_console_front[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

data "aws_iam_policy_document" "questdb_console_front_permissions" {
  count = var.enable_questdb_console ? 1 : 0

  # NOTE: this is the PARAMETER read grant, not the secret. The handler reads
  # the decrypted value at runtime (60s cache, fail-closed) — nothing
  # sensitive lands in env or Terraform state. Same param the portal uses.
  statement {
    sid       = "ReadControlSecret"
    effect    = "Allow"
    actions   = ["ssm:GetParameter"]
    resources = ["arn:aws:ssm:${var.aws_region}:${data.aws_caller_identity.current.account_id}:parameter/tickvault/${var.environment}/operator/control-secret"]
  }

  statement {
    sid       = "InvokeBackLambdaOnly"
    effect    = "Allow"
    actions   = ["lambda:InvokeFunction"]
    resources = [aws_lambda_function.questdb_console_proxy[0].arn]
  }
}

resource "aws_iam_role_policy" "questdb_console_front" {
  count  = var.enable_questdb_console ? 1 : 0
  name   = "tv-${var.environment}-qdb-console-front-permissions"
  role   = aws_iam_role.questdb_console_front[0].id
  policy = data.aws_iam_policy_document.questdb_console_front_permissions[0].json
}

resource "aws_lambda_function" "questdb_console_front" {
  count            = var.enable_questdb_console ? 1 : 0
  function_name    = "tv-${var.environment}-questdb-console-front"
  role             = aws_iam_role.questdb_console_front[0].arn
  runtime          = "python3.12"
  handler          = "handler.lambda_handler"
  filename         = data.archive_file.questdb_console_front[0].output_path
  source_code_hash = data.archive_file.questdb_console_front[0].output_base64sha256
  timeout          = 29 # API-GW caps integrations at 30s
  memory_size      = 256

  environment {
    variables = {
      BACK_FN_ARN          = aws_lambda_function.questdb_console_proxy[0].arn
      CONTROL_SECRET_PARAM = "/tickvault/${var.environment}/operator/control-secret"
    }
  }
}

# ─────────────────────────────────────────────────────────────────────────────
# API Gateway (HTTP API) → front Lambda. Same pattern as the operator portal:
# $default route, AWS_PROXY payload v2.0; real auth lives inside the handler
# (device-key / HMAC link token / session cookie — all constant-time).
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_apigatewayv2_api" "questdb_console" {
  count         = var.enable_questdb_console ? 1 : 0
  name          = "tv-${var.environment}-questdb-console"
  protocol_type = "HTTP"
}

resource "aws_apigatewayv2_integration" "questdb_console" {
  count                  = var.enable_questdb_console ? 1 : 0
  api_id                 = aws_apigatewayv2_api.questdb_console[0].id
  integration_type       = "AWS_PROXY"
  integration_uri        = aws_lambda_function.questdb_console_front[0].invoke_arn
  integration_method     = "POST"
  payload_format_version = "2.0"
}

resource "aws_apigatewayv2_route" "questdb_console" {
  count     = var.enable_questdb_console ? 1 : 0
  api_id    = aws_apigatewayv2_api.questdb_console[0].id
  route_key = "$default" # console shell + /exec + /exp + /open + /login — one Lambda
  target    = "integrations/${aws_apigatewayv2_integration.questdb_console[0].id}"
}

resource "aws_apigatewayv2_stage" "questdb_console" {
  count       = var.enable_questdb_console ? 1 : 0
  api_id      = aws_apigatewayv2_api.questdb_console[0].id
  name        = "$default"
  auto_deploy = true
}

resource "aws_lambda_permission" "questdb_console_apigw" {
  count         = var.enable_questdb_console ? 1 : 0
  statement_id  = "AllowApiGatewayInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.questdb_console_front[0].function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.questdb_console[0].execution_arn}/*/*"
}

# Per-Lambda error alarm (mirrors the operator-control alarm pattern).
resource "aws_cloudwatch_metric_alarm" "questdb_console_front_errors" {
  count               = var.enable_questdb_console ? 1 : 0
  alarm_name          = "tv-${var.environment}-questdb-console-front-errors"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  alarm_description   = "questdb-console front Lambda erroring — the console may be failing"
  dimensions          = { FunctionName = aws_lambda_function.questdb_console_front[0].function_name }
  treat_missing_data  = "notBreaching"
}

output "questdb_console_url" {
  value       = var.enable_questdb_console ? aws_apigatewayv2_stage.questdb_console[0].invoke_url : null
  description = "THE QuestDB console URL (API Gateway → front Lambda). Device-key gated inside the handler; opened one-click from the operator portal's 🗄 button."
}
