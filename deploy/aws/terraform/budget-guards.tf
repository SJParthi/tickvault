# =============================================================================
# Budget Guards — Daily Telegram Digest + Hard Auto-Stop
# =============================================================================
# Two Lambdas that protect the operator's monthly budget:
#
# 1. tv-prod-daily-budget-digest (runs 17:30 IST Mon-Fri = 12:00 UTC)
#    - Queries Cost Explorer for today's spend + month-to-date
#    - Publishes Telegram-formatted message to SNS tv-prod-alerts
#    - Operator sees daily message: "Today ₹X | MTD ₹Y / ₹2000 (Z%)"
#
# 2. tv-prod-hard-stop-guard (runs 17:00 IST EVERY day = 11:30 UTC)
#    - Force-stops the EC2 instance unconditionally if running
#    - Defends against EventBridge cron failure, manual mistakes,
#      and any case where the normal 16:30 IST stop didn't fire
#    - Publishes "auto-stop-guard fired" to Telegram only IF it had
#      to actually stop the instance (i.e. EventBridge missed)
#
# Cost: Both Lambdas under 1 invocation/day each — well within the
# AWS Lambda free tier (1M invocations/mo). Zero additional cost.
# =============================================================================

# --------- Daily Budget Digest Lambda ---------

data "archive_file" "tv_daily_budget_digest_zip" {
  type        = "zip"
  output_path = "${path.module}/.tv-daily-budget-digest.zip"
  source {
    content  = <<-PYEOF
import os, json, boto3
from datetime import datetime, timedelta, timezone

ce  = boto3.client('ce', region_name='us-east-1')  # Cost Explorer is us-east-1 only
sns = boto3.client('sns')

INR_PER_USD = 85    # rupee display rate (what you actually pay incl GST)
GST_MULT    = 1.18  # India GST 18%
# The REAL ceiling is the AWS Budget that auto-stops the box (budget.tf:20):
# $25/month measured on UnblendedCost (pre-GST USD). Comparing month-to-date
# USD against $25 makes the digest "% used" match the kill-switch EXACTLY —
# previously it compared rupees-with-GST against a separate ₹2000 number, so
# the percentage you saw did NOT line up with what actually stopped the box.
BUDGET_USD  = 25.0

# Friendly labels for the Cost Explorer SERVICE dimension (substring match).
SERVICE_LABELS = [
    ('Elastic Compute Cloud', 'EC2 compute'),
    ('EC2 - Other',           'EBS + transfer'),
    ('Virtual Private Cloud', 'Public IP / VPC'),
    ('CloudWatch',            'CloudWatch'),
    ('Simple Storage',        'S3 storage'),
    ('Simple Notification',   'SNS alerts'),
    ('Lambda',                'Lambda'),
    ('Key Management',        'KMS'),
]

def label_for(svc):
    for needle, nice in SERVICE_LABELS:
        if needle in svc:
            return nice
    return svc

def inr(usd):
    return usd * INR_PER_USD * GST_MULT

def get_total(start, end):
    r = ce.get_cost_and_usage(
        TimePeriod={'Start': start, 'End': end},
        Granularity='DAILY',
        Metrics=['UnblendedCost'],
    )
    return sum(float(d['Total']['UnblendedCost']['Amount']) for d in r['ResultsByTime'])

def get_by_service(start, end):
    r = ce.get_cost_and_usage(
        TimePeriod={'Start': start, 'End': end},
        Granularity='DAILY',
        Metrics=['UnblendedCost'],
        GroupBy=[{'Type': 'DIMENSION', 'Key': 'SERVICE'}],
    )
    agg = {}
    for d in r['ResultsByTime']:
        for g in d['Groups']:
            usd = float(g['Metrics']['UnblendedCost']['Amount'])
            if usd <= 0:
                continue
            key = label_for(g['Keys'][0])
            agg[key] = agg.get(key, 0.0) + usd
    return agg

def handler(event, context):
    today_utc = datetime.now(timezone.utc).date()
    yest_utc  = today_utc - timedelta(days=1)
    mtd_start = today_utc.replace(day=1)

    # Cost Explorer "end" is exclusive — yesterday's full day = today as end
    yday_usd = get_total(str(yest_utc), str(today_utc))
    mtd_usd  = get_total(str(mtd_start), str(today_utc))
    by_svc   = get_by_service(str(mtd_start), str(today_utc))

    pct = (mtd_usd / BUDGET_USD) * 100 if BUDGET_USD else 0
    emoji = '🟢' if pct < 50 else ('🟡' if pct < 80 else ('🟠' if pct < 100 else '🔴'))

    days_in_month  = (today_utc.replace(month=today_utc.month % 12 + 1, day=1) - timedelta(days=1)).day if today_utc.month < 12 else 31
    days_remaining = days_in_month - today_utc.day
    forecast_usd   = (mtd_usd / today_utc.day) * days_in_month if today_utc.day else mtd_usd

    lines = [
        f"{emoji} *AWS Cost — tickvault*",
        f"_Yesterday_:   ₹{inr(yday_usd):.0f}   ($${yday_usd:.2f})",
        f"_This month_:  ₹{inr(mtd_usd):.0f}   ($${mtd_usd:.2f})",
        f"_Of $25 stop-budget_: {pct:.0f}%",
        f"_Forecast EOM_: ₹{inr(forecast_usd):.0f}   ($${forecast_usd:.2f})",
        f"_Days left_:   {days_remaining}",
        "",
        "*Where it goes (this month):*",
    ]
    for nice, usd in sorted(by_svc.items(), key=lambda kv: -kv[1]):
        lines.append(f"  {nice:<16} ₹{inr(usd):.0f}  ($${usd:.2f})")
    if not by_svc:
        lines.append("  (no spend yet this month)")

    sns.publish(
        TopicArn=os.environ['ALERTS_TOPIC_ARN'],
        Subject='[BUDGET] daily AWS cost',
        Message="\n".join(lines),
    )
    return {'ok': True, 'mtd_usd': mtd_usd, 'pct': pct}
PYEOF
    filename = "index.py"
  }
}

resource "aws_iam_role" "tv_daily_budget_digest" {
  name = "tv-prod-daily-budget-digest-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "tv_daily_budget_digest" {
  name = "tv-prod-daily-budget-digest-policy"
  role = aws_iam_role.tv_daily_budget_digest.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ce:GetCostAndUsage", "ce:GetCostForecast"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = "sns:Publish"
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        Effect   = "Allow"
        Action   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "*"
      },
    ]
  })
}

resource "aws_lambda_function" "tv_daily_budget_digest" {
  function_name    = "tv-prod-daily-budget-digest"
  filename         = data.archive_file.tv_daily_budget_digest_zip.output_path
  source_code_hash = data.archive_file.tv_daily_budget_digest_zip.output_base64sha256
  role             = aws_iam_role.tv_daily_budget_digest.arn
  handler          = "index.handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  environment {
    variables = {
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
    }
  }
}

resource "aws_cloudwatch_log_group" "tv_daily_budget_digest" {
  name              = "/aws/lambda/tv-prod-daily-budget-digest"
  retention_in_days = 14
}

resource "aws_cloudwatch_event_rule" "tv_daily_budget_digest" {
  name                = "tv-prod-daily-budget-digest"
  description         = "Run daily budget digest at 17:30 IST (12:00 UTC) Mon-Fri"
  schedule_expression = "cron(0 12 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_target" "tv_daily_budget_digest" {
  rule      = aws_cloudwatch_event_rule.tv_daily_budget_digest.name
  target_id = "tv-daily-budget-digest"
  arn       = aws_lambda_function.tv_daily_budget_digest.arn
}

resource "aws_lambda_permission" "tv_daily_budget_digest_eventbridge" {
  statement_id  = "AllowExecutionFromEventBridge"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_daily_budget_digest.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_daily_budget_digest.arn
}

# --------- Hard Auto-Stop Guard Lambda ---------

data "archive_file" "tv_hard_stop_guard_zip" {
  type        = "zip"
  output_path = "${path.module}/.tv-hard-stop-guard.zip"
  source {
    content  = <<-PYEOF
import os, json, boto3

ec2 = boto3.client('ec2')
sns = boto3.client('sns')

INSTANCE_ID = os.environ['INSTANCE_ID']
ALERTS_ARN  = os.environ['ALERTS_TOPIC_ARN']

def handler(event, context):
    desc = ec2.describe_instances(InstanceIds=[INSTANCE_ID])
    state = desc['Reservations'][0]['Instances'][0]['State']['Name']
    if state in ('stopped', 'stopping', 'shutting-down', 'terminated'):
        print(f"Instance {INSTANCE_ID} already {state}, no-op.")
        return {'ok': True, 'noop': True, 'state': state}

    # Was running outside trading window — force stop + alert
    ec2.stop_instances(InstanceIds=[INSTANCE_ID])
    msg = (
        "🔴 *Hard Auto-Stop Guard Fired*\n"
        f"_instance_: `{INSTANCE_ID}`\n"
        f"_was_state_: {state}\n"
        "Daily 17:00 IST auto-stop guard found the instance still\n"
        "running. EventBridge 16:30 IST stop must have missed. Now\n"
        "stopped to protect the monthly budget.\n"
    )
    sns.publish(TopicArn=ALERTS_ARN, Subject='[BUDGET] hard auto-stop guard fired', Message=msg)
    return {'ok': True, 'noop': False, 'was_state': state}
PYEOF
    filename = "index.py"
  }
}

resource "aws_iam_role" "tv_hard_stop_guard" {
  name = "tv-prod-hard-stop-guard-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "tv_hard_stop_guard" {
  name = "tv-prod-hard-stop-guard-policy"
  role = aws_iam_role.tv_hard_stop_guard.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ec2:DescribeInstances", "ec2:StopInstances"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = "sns:Publish"
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        Effect   = "Allow"
        Action   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "*"
      },
    ]
  })
}

resource "aws_lambda_function" "tv_hard_stop_guard" {
  function_name    = "tv-prod-hard-stop-guard"
  filename         = data.archive_file.tv_hard_stop_guard_zip.output_path
  source_code_hash = data.archive_file.tv_hard_stop_guard_zip.output_base64sha256
  role             = aws_iam_role.tv_hard_stop_guard.arn
  handler          = "index.handler"
  runtime          = "python3.12"
  timeout          = 30
  memory_size      = 128
  environment {
    variables = {
      INSTANCE_ID      = aws_instance.tv_app.id
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
    }
  }
}

resource "aws_cloudwatch_log_group" "tv_hard_stop_guard" {
  name              = "/aws/lambda/tv-prod-hard-stop-guard"
  retention_in_days = 14
}

# Run EVERY day (including weekends) at 17:00 IST = 11:30 UTC.
# Why every day, not just Mon-Fri: a manually-started instance over
# the weekend should also auto-stop, not run all night.
resource "aws_cloudwatch_event_rule" "tv_hard_stop_guard" {
  name                = "tv-prod-hard-stop-guard"
  description         = "Force-stop instance every day at 17:00 IST (11:30 UTC) — budget safety net"
  schedule_expression = "cron(30 11 * * ? *)"
}

resource "aws_cloudwatch_event_target" "tv_hard_stop_guard" {
  rule      = aws_cloudwatch_event_rule.tv_hard_stop_guard.name
  target_id = "tv-hard-stop-guard"
  arn       = aws_lambda_function.tv_hard_stop_guard.arn
}

resource "aws_lambda_permission" "tv_hard_stop_guard_eventbridge" {
  statement_id  = "AllowExecutionFromEventBridge"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.tv_hard_stop_guard.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.tv_hard_stop_guard.arn
}
