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
# 2. tv-prod-hard-stop-guard (runs HOURLY, every day)
#    - Force-stops the EC2 instance if running OUTSIDE the Mon-Fri
#      08:30-16:30 IST up-window (missed 16:30 stop / manual start)
#    - IN-window (GAP 1, 2026-07-03): if month-to-date spend has crossed
#      BUDGET_KILL_USD ($55 — same line as budget.tf limit_amount), it
#      stops the box AND disables the tv-prod-daily-start EventBridge
#      rule, because the native AWS Budget stop-actions fire only ONCE
#      per month-crossing — without this, the next morning's start cron
#      restarts the box and it runs daily, unkilled, for the rest of
#      the month. Cost Explorer errors fail-safe (page, never disable).
#    - Source lives in crates/aws-lambdas/src/hard_stop_guard.rs (the
#      Rust port, rust-only phase 2b-2 wave 2 — the former
#      deploy/aws/lambda/hard-stop-guard/ handler dir was deleted in the
#      same PR; see the Hard Auto-Stop Guard section comment below)
#
# Cost: Both Lambdas under 1 invocation/day each — well within the
# AWS Lambda free tier (1M invocations/mo). Zero additional cost.
# =============================================================================

# --------- Daily Budget Digest Lambda ---------
#
# 2026-07-18 (rust-only phase 2b-1): the inline legacy heredoc was PORTED to
# Rust — crates/aws-lambdas/src/budget_digest.rs (lib logic + unit tests) +
# src/bin/daily_budget_digest.rs (thin bootstrap bin). Behavior parity:
# same Cost Explorer queries (us-east-1, DAILY UnblendedCost, exclusive
# end), same INR_PER_USD=85 / GST_MULT=1.18 / BUDGET_USD constants
# (BUDGET_USD=25 since the 2026-07-19 sub-1K ruling step, was 55)
# (KEEP IN SYNC with budget.tf limit_amount), same emoji thresholds, same
# Telegram line format, same '[BUDGET] daily AWS cost' subject, same
# {'ok','mtd_usd','pct'} return. The zip is built in CI by the
# build-lambdas job (terraform-apply.yml) and downloaded into
# ${path.module}/.lambda-zips/ before plan/apply; source_code_hash is a
# digest of the Rust SOURCE (Rust builds are not bit-reproducible, so
# hashing the zip would churn every build with zero source change).
# 2026-07-18 (hostile-review r1 N3): a LOCAL operator `terraform plan`
# requires running the CI build step's cargo-lambda command first (the
# `file(".lambda-zips/source.digest")` reads fail otherwise) — CI-only
# planning is the intended contract; and a toolchain/cargo-lambda version
# bump alone does NOT change the digest (binaries redeploy on the next
# source / Cargo.lock change). Applies to all five .lambda-zips lambdas
# (the four 2b-1 ports + operator-control, rust-only phase 2b-3 2026-07-18).

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
  filename         = "${path.module}/.lambda-zips/daily-budget-digest.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))
  role             = aws_iam_role.tv_daily_budget_digest.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
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

# 2026-07-18 (rust-only phase 2b-2 wave 2): ported from the legacy handler
# (deploy/aws/lambda/hard-stop-guard/handler.py, deleted in the same PR) to
# the Rust binary `hard-stop-guard` in crates/aws-lambdas
# (src/hard_stop_guard.rs — GAP 1 breach->stop+disable logic, 34 tests
# ported 1:1). Packaged by the terraform-apply workflow's cargo-lambda
# build step into .lambda-zips/ (same idiom as tv_daily_budget_digest
# above).

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
        # 2026-08-10 (security review): Describe* has NO resource-level
        # condition in IAM, so it genuinely must stay "*". StopInstances does
        # not — and bundling the two under one "*" granted this role the
        # ability to stop ANY instance in the account, not just the trading
        # box. Split, and the mutating half scoped by the Name tag exactly as
        # the instance-profile self-stop policy in main.tf already does.
        Effect   = "Allow"
        Action   = ["ec2:DescribeInstances"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = ["ec2:StopInstances"]
        Resource = "arn:aws:ec2:${var.aws_region}:*:instance/*"
        Condition = {
          StringEquals = {
            "ec2:ResourceTag/Name" = "tv-${var.environment}-app"
          }
        }
      },
      {
        # Read-only MTD spend for the hourly cost ping AND the GAP 1
        # in-window breach check. Fail-safe: a Cost Explorer error never
        # stops the box or disables the start rule (page-only).
        Effect   = "Allow"
        Action   = ["ce:GetCostAndUsage"]
        Resource = "*"
      },
      {
        # GAP 1 (post-breach morning restart): on MTD >= $55 the Lambda
        # disables the morning start cron so the box cannot be restarted
        # daily for the rest of the month after a budget kill. Least
        # privilege — scoped to the ONE tv-prod-daily-start rule ARN.
        Sid      = "DisableDailyStartRuleOnBudgetBreach"
        Effect   = "Allow"
        Action   = ["events:DisableRule"]
        Resource = aws_cloudwatch_event_rule.daily_start.arn
      },
      {
        Effect   = "Allow"
        Action   = "sns:Publish"
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        # 2026-07-09 (Telegram noise N2 — change-only running pings): the
        # hourly in-window ping now fires only on a CHANGE (spend bucket /
        # month rollover / cost-check edge). State = ONE SSM String param.
        # Least privilege — scoped to that single parameter ARN (the
        # events:DisableRule idiom above); created lazily by
        # PutParameter(Overwrite=true), no seed resource needed. NOT under
        # the banned /tickvault/*/groww/* namespace.
        Sid      = "ChangeOnlyBudgetPingState"
        Effect   = "Allow"
        Action   = ["ssm:GetParameter", "ssm:PutParameter"]
        Resource = "arn:aws:ssm:${var.aws_region}:${data.aws_caller_identity.current.account_id}:parameter/tickvault/${var.environment}/budget-guard/ping-state"
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
  filename         = "${path.module}/.lambda-zips/hard-stop-guard.zip"
  source_code_hash = chomp(file("${path.module}/.lambda-zips/source.digest"))
  role             = aws_iam_role.tv_hard_stop_guard.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  timeout          = 30
  memory_size      = 128
  environment {
    variables = {
      INSTANCE_ID      = aws_instance.tv_app.id
      ALERTS_TOPIC_ARN = aws_sns_topic.tv_alerts.arn
      # GAP 1: the morning start cron the Lambda disables on a breach.
      START_RULE_NAME = aws_cloudwatch_event_rule.daily_start.name
      # KEEP IN SYNC with budget.tf limit_amount ("35") + the digest's
      # BUDGET_USD above — all three MUST agree on the kill line.
      # ($55 -> $25 on 2026-07-19 per the sub-1K ruling step in budget.tf;
      # $25 -> $35 on 2026-07-31 per the operator ruling recorded verbatim in
      # budget.tf — the live budget had breached at 109.9% and both AUTOMATIC
      # STOP_EC2 actions were stuck in EXECUTION_FAILURE against the prod box;
      # hard_stop_guard.rs DEFAULT_BUDGET_KILL_USD=55.0 remains the
      # env-missing FALLBACK only — this env var is always injected, so the
      # runtime kill line is $35; aligning the fallback const is a flagged
      # follow-up, fail direction = kills later, never earlier.)
      # $35 -> $100 on 2026-08-08 per the operator ruling recorded verbatim in
      # budget.tf (Quote 13 — r8g.xlarge for the 13-timeframe + tick-retention
      # requirement; the bill's high estimate is $73.60 and the actions fire at
      # 90%/100%, so a ceiling under ~$82 would stop the box mid-session).
      # $100 -> $130 on 2026-08-19 per the operator ruling recorded verbatim in
      # budget.tf (Quote 17 — gp3 IOPS 3000->6000 + throughput 125->500 adds
      # $30.00/mo, moving the bill's high estimate $82.72 -> $112.72; the actions
      # fire at 90%/100%, so a $100 ceiling would put the 90% line at $90, BELOW
      # the new bill, and stop the trading box mid-session. $130 puts it at $117 —
      # $4.28 of room, thinner than the $7.28 this change was called in to fix.)
      BUDGET_KILL_USD = "150"
      # 2026-07-09: change-only ping state (matches the IAM statement's
      # single-parameter scope above).
      PING_STATE_PARAM = "/tickvault/${var.environment}/budget-guard/ping-state"
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
# overnight/weekend); INSIDE the window it checks the budget hourly but PINGS
# ONLY ON CHANGE (spend crossed a 10% bucket / month rollover / cost-check
# edge — 2026-07-09 Telegram noise N2; the 17:30 IST daily digest above stays
# the end-of-day summary), UNLESS MTD spend has crossed the $55 kill line —
# then it stops the box + disables the morning start cron (GAP 1, 2026-07-03).
# Hourly = the box can over-run the budget by at most ~1 EC2-hour (~$0.064)
# before this catches it, plus the native AWS Budget Action (budget.tf) stops
# at 90%/100% spend regardless (but only ONCE per month-crossing — this Lambda
# is what keeps the box down for the rest of the month).
# 1 invocation/hour is well within the Lambda free tier (1M/mo).
resource "aws_cloudwatch_event_rule" "tv_hard_stop_guard" {
  name                = "tv-prod-hard-stop-guard"
  description         = "Hourly out-of-window force-stop + in-window change-only budget ping — budget never-cross safety net"
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

# ---------------------------------------------------------------------------
# Watch the watchman (2026-08-25, operator "Fix wbrytjonf dude oaku" — the
# §2.3f dated authorization in dhan-rest-only-noise-lock-2026-07-14.md).
#
# This Lambda had NO Errors alarm. It was one of SIX in that state out of 13 —
# not the one the authorizing message claimed, which is corrected in §2.3f.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "hard_stop_guard_errors" {
  alarm_name          = "tv-${var.environment}-hard-stop-guard-errors"
  alarm_description   = "The HARD STOP GUARD itself FAILED. This is the hourly Lambda that force-stops the box when it is running outside its authorized window or over budget. Its silence cuts BOTH ways: a box left running bills unnoticed, and a guard that is spuriously stopping a healthy box is equally invisible. Note the standing caveat - the budget's own STOP_EC2_INSTANCES actions have been recorded in EXECUTION_FAILURE since 2026-07-31 and cannot be re-checked with the current IAM identity, so this guard may be the only stop path that actually works. Triage: read the Lambda log group."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.tv_hard_stop_guard.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  # NO ok_actions (round-14): a post-ALARM auto-OK only means the Errors
  # datapoint aged out of the lookback, never that anything was fixed.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# Watch the watchman (2026-08-25, operator "Fix wbrytjonf dude oaku" — the
# §2.3f dated authorization in dhan-rest-only-noise-lock-2026-07-14.md).
#
# This Lambda had NO Errors alarm. It was one of SIX in that state out of 13 —
# not the one the authorizing message claimed, which is corrected in §2.3f.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "daily_budget_digest_errors" {
  alarm_name          = "tv-${var.environment}-daily-budget-digest-errors"
  alarm_description   = "The DAILY BUDGET DIGEST itself FAILED. Its failure mode is a digest that simply stops arriving - which reads exactly like a quiet day, so nothing else reports it. Spend visibility is the first thing lost. Triage: read the Lambda log group."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.tv_daily_budget_digest.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  # NO ok_actions (round-14): a post-ALARM auto-OK only means the Errors
  # datapoint aged out of the lookback, never that anything was fixed.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 2026-08-25 — "did the kill-switch RUN?" (the gap behind four rule-file rows)
#
# CORRECTION FIRST, because the record has been scarier than the code for a
# month. budget.tf, daily-universe-scope-expansion §7 and two Quote sections
# all carry a FLAGGED, UNRESOLVED note reading, in substance, "if both
# STOP_EC2_INSTANCES budget actions are still in EXECUTION_FAILURE then the
# kill switch does not fire AT ALL". That sentence names the AWS-NATIVE budget
# action and then generalises to "the kill switch", which is wrong: this
# account has TWO independent switches, and the second one is ours.
#
# `tv_hard_stop_guard` above reads month-to-date spend from Cost Explorer every
# hour, and at `>= BUDGET_KILL_USD` it stops the instance, disables the morning
# auto-start rule, and pages — see `hard_stop_guard.rs::classify` ("breach_stop"
# / "cost_unknown" / "below_budget") and `execute_breach_stop`. Its IAM policy
# carries `ec2:StopInstances` scoped by the `tv-<env>-app` Name tag,
# `ce:GetCostAndUsage` and `events:DisableRule`. It does not consult the native
# budget action and does not care whether that action works. So the honest
# statement is "the AWS-NATIVE action is unverifiable", never "the kill switch
# may not fire" — and whether the native one works is now a nice-to-know rather
# than the most serious open item on the account.
#
# THE GAP THAT IS REAL, and that nobody was looking at while everyone worried
# about the native action: NOTHING checks that OUR switch still runs. Its
# Errors alarm (added 2026-08-25 with the other twelve) is structurally blind to
# a dropped schedule — zero invocations produce zero errors, so a silently
# disabled EventBridge rule reads as a permanently healthy Lambda. That is the
# 2026-07-02 repo-wide scheduler-drop class, which this repo has already been
# bitten by twice, and it applies with more force here than anywhere else: a
# kill-switch that stopped being invoked is indistinguishable from a kill-switch
# that has nothing to do.
#
# 6-hour window, not the 24h the token-minter uses. That alarm watches a DAILY
# mint where 24h is the natural unit; this Lambda fires HOURLY, so a 6h window
# expects six invocations and still cannot false-page on a single miss, while
# cutting detection from ~a day to ~a quarter of one. For the component whose
# whole job is to stop a runaway bill, a day of blindness is the wrong trade.
#
# ok_actions = [] deliberately: recovery here means "a datapoint appeared
# again", which is worth seeing in the console and is not worth a second page.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "tv_hard_stop_guard_not_invoked" {
  alarm_name          = "tv-${var.environment}-hard-stop-guard-not-invoked"
  alarm_description   = "The hourly budget kill-switch did NOT RUN in the last 6h - its EventBridge schedule was dropped or disabled (the 2026-07-02 scheduler-drop class). This is the guard that stops the box on a budget breach; while it is not running there is no spend ceiling in force at all, and its Errors alarm cannot see this (no invocation = no error)."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Invocations"
  namespace           = "AWS/Lambda"
  period              = 21600
  statistic           = "Sum"
  threshold           = 1
  # breaching: a missing Invocations datapoint IS the condition being detected.
  treat_missing_data = "breaching"
  dimensions = {
    FunctionName = aws_lambda_function.tv_hard_stop_guard.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = []
}


# ---------------------------------------------------------------------------
# 2026-08-25 - "did the digest RUN?" (found by the new guard)
#
# The tempting exemption - "the operator notices a missing daily Telegram" - is
# refused deliberately. It is the same reasoning this repo has rejected twice
# before: noticing an ABSENCE requires remembering to expect something, which
# is precisely what people stop doing on the days it matters. The digest is
# also the only routine surface on which a cost trend is visible at all; the
# hourly kill-switch reads spend but only speaks at the threshold.
#
# 24h window: MON-FRI 17:30 IST cron, one invocation expected per weekday.
#
# ok_actions = [] - recovery here means "a datapoint appeared again", which is
# worth seeing in the console and is not worth a second page.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "tv_daily_budget_digest_not_invoked" {
  alarm_name          = "tv-${var.environment}-daily-budget-digest-not-invoked"
  alarm_description   = "The daily spend digest did NOT RUN in the last 24h - its EventBridge schedule was dropped or disabled (the 2026-07-02 scheduler-drop class). The digest is the operator-facing view of AWS spend; while it is not running the bill is unobserved between the hourly kill-switch checks, and its Errors alarm cannot see this (no invocation = no error)."
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
    FunctionName = aws_lambda_function.tv_daily_budget_digest.function_name
  }
  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = []
}
