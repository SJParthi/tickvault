# =============================================================================
# Daily AWS waste check (noise lock §2.18, owner card 2026-11-11
# "Yes, daily waste check (Recommended)")
# =============================================================================
# Why: two EBS volumes attached to nothing (600 GB and 30 GB) and two old
# snapshots billed about $21 from 1 to 11 Nov 2026 and no message named them.
#
# Once a day at 18:07 IST (12:37 UTC, every day, outside 09:00-15:40 IST) a
# Rust Lambda (crates/aws-lambdas/src/waste_check.rs) reads, READ-ONLY:
#   - ec2:DescribeVolumes   (volumes in state available; reported once seen
#     unattached in runs 2 IST days apart)
#   - ec2:DescribeSnapshots (owner self, older than 30 days)
#   - ec2:DescribeAddresses (Elastic IPs with no association)
#   - at most ONE ce:GetCostAndUsage per IST day (month-to-date by service:
#     the top-5 context line; $0.01 a call, about $0.30 a month; SDK retries
#     are off in the client, so one call is one billed request)
# and sends ONE plain-English message through the alerts topic only when the
# finding set gains an item since the previous run. Every run writes
# s3://tv-<env>-cold/cost-audit/<date>.json.
#
# STATE AND DAY CLAIMS (all in the cold bucket, nothing deleted, versioning
# unchanged):
#   - waste-check/state.json: the previous run's finding ids and the first
#     date each unattached volume was seen (no size cap, unlike one SSM
#     parameter). Read with GetObject, found with a ListBucket limited by an
#     s3:prefix condition to that one key (without a list grant, a GetObject
#     of a missing key answers 403, which looks like a real denial).
#   - waste-check/claims/<date>-cost-query and -message: create-only objects
#     (PutObject with If-None-Match: *), so a re-run, a test invoke or a
#     duplicate EventBridge delivery the same IST day cannot make a second
#     paid query or send a second message.
#
# POWER: it can only look. No delete, release, modify, stop or terminate
# action appears in its role; crates/aws-lambdas/tests/waste_check_wiring_guard.rs
# fails the build if one does. The ec2 Describe actions and
# ce:GetCostAndUsage do not support resource-level permissions, so they take
# Resource "*" (read-only by definition).
#
# Retries: maximum_retry_attempts = 0, so a failed run is never re-run by
# Lambda. At most one message a day is held by the message claim, not by the
# retry setting. Inside a run the SNS publish and the day claims make ONE
# attempt each (SDK retries off), and a publish whose outcome is unknown
# (timeout, lost reply, 5xx) keeps the "sent" state: a possible miss, never a
# repeat. A missed day is caught by the not-invoked alarm. §2.18 allows
# no other alarm, so a run that FAILS is visible only in its log group and as
# a missing cost-audit file for that day.
# =============================================================================

resource "aws_iam_role" "aws_waste_check" {
  name = "tv-${var.environment}-aws-waste-check-lambda"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "aws_waste_check" {
  name = "tv-${var.environment}-aws-waste-check-policy"
  role = aws_iam_role.aws_waste_check.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "DescribeWasteReadOnly"
        Effect   = "Allow"
        Action   = ["ec2:DescribeVolumes", "ec2:DescribeSnapshots", "ec2:DescribeAddresses"]
        Resource = "*"
      },
      {
        Sid      = "OneMonthToDateCostQuery"
        Effect   = "Allow"
        Action   = ["ce:GetCostAndUsage"]
        Resource = "*"
      },
      {
        Sid      = "FindWasteState"
        Effect   = "Allow"
        Action   = ["s3:ListBucket"]
        Resource = aws_s3_bucket.tv_cold.arn
        Condition = {
          StringEquals = { "s3:prefix" = "waste-check/state.json" }
        }
      },
      {
        Sid      = "ReadWasteState"
        Effect   = "Allow"
        Action   = ["s3:GetObject"]
        Resource = "${aws_s3_bucket.tv_cold.arn}/waste-check/state.json"
      },
      {
        Sid    = "WriteWasteStateClaimsAndAudit"
        Effect = "Allow"
        Action = ["s3:PutObject"]
        Resource = [
          "${aws_s3_bucket.tv_cold.arn}/waste-check/state.json",
          "${aws_s3_bucket.tv_cold.arn}/waste-check/claims/*",
          "${aws_s3_bucket.tv_cold.arn}/cost-audit/*",
        ]
      },
      {
        Sid      = "PublishWasteMessage"
        Effect   = "Allow"
        Action   = "sns:Publish"
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        # Its OWN log group only (2026-11-11 review): a region-wide grant
        # would let this role write coded ERROR lines into the app log
        # groups and trip their metric-filter pages. No CreateLogGroup:
        # terraform creates the group below.
        Sid      = "WriteOwnLogs"
        Effect   = "Allow"
        Action   = ["logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "${aws_cloudwatch_log_group.aws_waste_check.arn}:*"
      },
    ]
  })
}

resource "aws_lambda_function" "aws_waste_check" {
  function_name    = "tv-${var.environment}-aws-waste-check"
  filename         = "${path.module}/.lambda-zips/aws-waste-check.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))
  role             = aws_iam_role.aws_waste_check.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  timeout          = 60
  memory_size      = 128
  environment {
    variables = {
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
      AUDIT_BUCKET     = aws_s3_bucket.tv_cold.bucket
    }
  }
  tags = {
    Name    = "tv-${var.environment}-aws-waste-check"
    Project = "tickvault"
    Layer   = "L5-AUDIT"
  }
}

# No automatic re-run of a failed invocation (the day claims hold the
# one-message and one-query bounds even if one ran).
resource "aws_lambda_function_event_invoke_config" "aws_waste_check" {
  function_name          = aws_lambda_function.aws_waste_check.function_name
  maximum_retry_attempts = 0
}

resource "aws_cloudwatch_log_group" "aws_waste_check" {
  name              = "/aws/lambda/tv-${var.environment}-aws-waste-check"
  retention_in_days = 14
  tags = {
    Project = "tickvault"
    Layer   = "L5-AUDIT"
  }
}

resource "aws_cloudwatch_event_rule" "aws_waste_check" {
  name                = "tv-${var.environment}-aws-waste-check"
  description         = "18:07 IST daily read-only AWS waste check (noise lock 2.18): unattached disks, old snapshots, idle IPs, top 5 costs"
  schedule_expression = "cron(37 12 * * ? *)"
  state               = "ENABLED"
}

resource "aws_cloudwatch_event_target" "aws_waste_check" {
  rule      = aws_cloudwatch_event_rule.aws_waste_check.name
  target_id = "tv-aws-waste-check"
  arn       = aws_lambda_function.aws_waste_check.arn
}

resource "aws_lambda_permission" "aws_waste_check" {
  statement_id  = "AllowExecutionFromEventBridgeAwsWasteCheck"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.aws_waste_check.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.aws_waste_check.arn
}

# The ONE alarm §2.18 allows: the check did not run for two days in a row (it
# runs every day, weekends included). HONEST LIMIT (Assumed from CloudWatch's
# missing-data rules, not observed): with treat_missing_data = "breaching" a
# freshly created alarm sees two days with no Invocations datapoint and goes
# to ALARM at once, so the first terraform apply sends one false "did not
# run" page. It needs no action: ok_actions is empty, the alarm settles once
# two days have an invocation, and it pages again only if the schedule is
# dropped. Shipping it with actions off would need a manual step to arm it.
resource "aws_cloudwatch_metric_alarm" "aws_waste_check_not_invoked" {
  alarm_name          = "tv-${var.environment}-aws-waste-check-not-invoked"
  alarm_description   = "The daily AWS waste check did NOT RUN for 2 days - its EventBridge schedule was dropped or disabled (the 2026-07-02 scheduler-drop class). While it is not running, a disk or IP billing money while doing nothing goes unreported. Right after the alarm is first created it fires once by itself (no run yet); that page needs no action."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2
  datapoints_to_alarm = 2
  metric_name         = "Invocations"
  namespace           = "AWS/Lambda"
  period              = 86400
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "breaching"
  dimensions = {
    FunctionName = aws_lambda_function.aws_waste_check.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = []
}
