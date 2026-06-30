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
# The REAL ceiling is the AWS Budget that auto-stops the box (budget.tf
# limit_amount): $55/month measured on UnblendedCost (pre-GST USD, TOTAL
# account spend — the budget cost_filter was removed 2026-06-30). Comparing
# month-to-date USD against $55 makes the digest "% used" match the kill-switch
# EXACTLY. KEEP THIS IN SYNC with budget.tf limit_amount — the digest reads
# BUDGET_USD, the native Budget Action + killswitch fire at limit_amount.
BUDGET_USD  = 55.0

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
import os, boto3
from datetime import datetime, timedelta, timezone

ec2 = boto3.client('ec2')
sns = boto3.client('sns')

INSTANCE_ID = os.environ['INSTANCE_ID']
ALERTS_ARN  = os.environ['ALERTS_TOPIC_ARN']

# IST = UTC+5:30. Up-window = Mon-Fri 08:30-16:30 IST (the EventBridge
# start/stop schedule). The guard now runs HOURLY (2026-06-30) so the box can
# never bill a full overnight if the 16:30 stop missed; it force-stops the box
# any time it runs OUTSIDE the up-window, and NEVER touches it DURING the window
# (a strict no-op in-window so it can't kill a live market-hours session).
def in_up_window(now_utc):
    ist = now_utc + timedelta(hours=5, minutes=30)
    dow = ist.weekday()              # 0=Mon .. 6=Sun
    hhmm = ist.hour * 100 + ist.minute
    return dow <= 4 and 830 <= hhmm <= 1630

def mtd_usd():
    # Best-effort Cost Explorer MTD (us-east-1). Never fails the stop logic.
    try:
        ce = boto3.client('ce', region_name='us-east-1')
        today = datetime.now(timezone.utc).date()
        start = today.replace(day=1)
        r = ce.get_cost_and_usage(
            TimePeriod={'Start': str(start), 'End': str(today)},
            Granularity='MONTHLY', Metrics=['UnblendedCost'],
        )
        return sum(float(d['Total']['UnblendedCost']['Amount']) for d in r['ResultsByTime'])
    except Exception as e:  # noqa: BLE001 — never let cost lookup break the stop
        print(f"MTD lookup failed (non-fatal): {e}")
        return None

def handler(event, context):
    now_utc = datetime.now(timezone.utc)
    desc = ec2.describe_instances(InstanceIds=[INSTANCE_ID])
    inst = desc['Reservations'][0]['Instances'][0]
    state = inst['State']['Name']

    if state in ('stopped', 'stopping', 'shutting-down', 'terminated'):
        print(f"Instance {INSTANCE_ID} already {state}, no-op.")
        return {'ok': True, 'noop': True, 'state': state}

    if in_up_window(now_utc):
        # Running DURING the trading window — expected. Hourly "still running"
        # cost ping so the operator sees the box is up + spend so far. NEVER
        # stop in-window (would kill a live market-hours session).
        launch = inst.get('LaunchTime')
        hrs_up = (now_utc - launch).total_seconds() / 3600 if launch else 0.0
        usd = mtd_usd()
        usd_str = f"~$${usd:.2f}" if usd is not None else "n/a"
        sns.publish(
            TopicArn=ALERTS_ARN,
            Subject='[BUDGET] box still running',
            Message=(
                "🟢 *Box running (in window)*\n"
                f"_instance_: `{INSTANCE_ID}`\n"
                f"_up_: {hrs_up:.1f}h this boot\n"
                f"_MTD spend_: {usd_str} / $55 stop-budget\n"
                "In the 08:30-16:30 IST trading window — left running.\n"
            ),
        )
        return {'ok': True, 'noop': True, 'in_window': True, 'state': state}

    # Running OUTSIDE the up-window — force stop + alert (the never-cross guard).
    ec2.stop_instances(InstanceIds=[INSTANCE_ID])
    msg = (
        "🔴 *Hard Auto-Stop Guard Fired*\n"
        f"_instance_: `{INSTANCE_ID}`\n"
        f"_was_state_: {state}\n"
        "Hourly out-of-window guard found the box running OUTSIDE the\n"
        "08:30-16:30 IST Mon-Fri window. EventBridge 16:30 stop (or a\n"
        "manual start) left it up. Now stopped to protect the budget.\n"
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
        # Read-only MTD spend for the hourly "still running" cost ping.
        # Best-effort: the Lambda's stop logic never depends on this.
        Effect   = "Allow"
        Action   = ["ce:GetCostAndUsage"]
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

# Run HOURLY, every day (2026-06-30 — was once-daily 17:00 IST). The Lambda is
# window-aware: OUTSIDE the Mon-Fri 08:30-16:30 IST up-window it force-stops a
# running box (so a missed 16:30 stop or a manual start can NEVER bill a full
# overnight/weekend); INSIDE the window it is a strict no-op except for an
# hourly "box still running — Xh, ~$Y MTD" cost ping. Hourly = the box can over-
# run the budget by at most ~1 EC2-hour (~$0.064) before this catches it, plus
# the native AWS Budget Action (budget.tf) stops at 90%/100% spend regardless.
# 1 invocation/hour is well within the Lambda free tier (1M/mo).
resource "aws_cloudwatch_event_rule" "tv_hard_stop_guard" {
  name                = "tv-prod-hard-stop-guard"
  description         = "Hourly out-of-window force-stop + in-window running cost ping — budget never-cross safety net"
  schedule_expression = "cron(0 * * * ? *)"
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
