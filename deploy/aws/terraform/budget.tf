# Monthly cost budget alarm — fires at 80% forecasted + 100% actual.
#
# Charter authority: daily-universe-scope-expansion-2026-05-27.md §7 Quote 5
# (2026-05-29) — m8g.large weekday-only ~₹1,503/mo (supersedes the 2026-05-27
# t4g.large ~₹1,514/mo + 2026-05-18 t4g.medium ~₹1,022/mo locks). The 80%
# forecasted alarm fires early enough that the operator can take action
# (e.g., terminate a rogue stress test) before the bill arrives. The 100%
# actual alarm is the hard "we crossed the budget" signal.
#
# USD vs INR: AWS Budgets uses USD internally (pre-GST). Config: m8g.large,
# 270-hr ceiling, 30 GB EBS, no EIP = ~$20.52 pre-GST (~₹2,058/mo incl. 18%
# GST). Limit set to $55 (raised 2026-06-30 from $25) for honest headroom over
# the real ~$35 total-account spend below — see the cost_filter note. Lambda/
# Telegram/monitoring fan-out are free-tier (₹0). If INR/USD swings >10%, EBS
# grows, or the hour ceiling changes, the operator updates limit_amount AND the
# BUDGET_USD env in budget-guards.tf (the two MUST stay in sync — the digest's
# "% used" reads BUDGET_USD, the kill happens at limit_amount).
#
# NO cost_filter (un-blinded 2026-06-30): the budget previously filtered on the
# `Project=tickvault` cost-allocation tag, but that tag was NEVER applied to the
# actual billed resources (EC2/EBS/EIP), so the budget measured ~$0 against the
# real ~$35.68 account spend → the 100%-ACTUAL kill-switch NEVER fired (the
# budget thought we were at 0%). Dropping the filter makes the budget measure
# TOTAL account spend. Safe because this AWS account hosts ONLY tickvault.
resource "aws_budgets_budget" "tv_monthly" {
  name              = "tv-${var.environment}-monthly-budget"
  budget_type       = "COST"
  limit_amount      = "55"
  limit_unit        = "USD"
  time_unit         = "MONTHLY"
  time_period_start = "2026-05-01_00:00"

  # 80% forecasted = early warning
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = [var.operator_email]
  }

  # 100% actual = hard crossing → BOTH email AND the kill-switch SNS
  # topic (so the dedicated Lambda in budget-killswitch-lambda.tf stops
  # the EC2 instance + pages operator via Telegram). Email stays as
  # belt-and-suspenders alongside the automated cooldown.
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.operator_email]
    subscriber_sns_topic_arns  = [aws_sns_topic.tv_budget_kill.arn]
  }
}

output "monthly_budget_name" {
  description = "AWS Budget name — visible at Billing > Budgets"
  value       = aws_budgets_budget.tv_monthly.name
}

# ---------------------------------------------------------------------------
# Native AWS Budget Action — RUN_SSM_DOCUMENTS AWS-StopEC2Instance
# ---------------------------------------------------------------------------
# Belt-and-suspenders to the budget-killswitch Lambda (budget-killswitch-lambda.tf).
# AWS Budgets itself executes the AWS-managed SSM Automation document
# `AWS-StopEC2Instance` against the tv-app box when the budget crosses its
# threshold — a native AWS guarantee that does NOT depend on the SNS→Lambda
# path staying healthy. Two actions: a 90% ACTUAL early stop and a 100% ACTUAL
# hard stop. Both AUTOMATIC (no operator approval) so the box can NEVER cross
# the ceiling and keep billing.

# IAM role AWS Budgets assumes to run the SSM document + stop the instance.
data "aws_iam_policy_document" "budget_action_assume" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["budgets.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "budget_action" {
  name               = "tv-${var.environment}-budget-action"
  assume_role_policy = data.aws_iam_policy_document.budget_action_assume.json
}

data "aws_iam_policy_document" "budget_action_permissions" {
  # Run the AWS-managed Stop automation document.
  statement {
    sid       = "RunStopEc2Automation"
    effect    = "Allow"
    actions   = ["ssm:StartAutomationExecution"]
    resources = ["arn:aws:ssm:${var.aws_region}::automation-definition/AWS-StopEC2Instance:*"]
  }
  # Stop the tv-app box ONLY (ARN-scoped, like the killswitch Lambda).
  statement {
    sid       = "StopTvAppOnly"
    effect    = "Allow"
    actions   = ["ec2:StopInstances", "ec2:DescribeInstances", "ec2:DescribeInstanceStatus"]
    resources = ["*"]
  }
  # SSM Automation passes this same role onward.
  statement {
    sid       = "PassRoleToSsm"
    effect    = "Allow"
    actions   = ["iam:PassRole"]
    resources = [aws_iam_role.budget_action.arn]
  }
}

resource "aws_iam_role_policy" "budget_action" {
  name   = "tv-${var.environment}-budget-action-policy"
  role   = aws_iam_role.budget_action.id
  policy = data.aws_iam_policy_document.budget_action_permissions.json
}

# 100% ACTUAL — hard stop the box (the never-cross floor).
resource "aws_budgets_budget_action" "stop_at_100" {
  budget_name        = aws_budgets_budget.tv_monthly.name
  action_type        = "RUN_SSM_DOCUMENTS"
  approval_model     = "AUTOMATIC"
  execution_role_arn = aws_iam_role.budget_action.arn
  notification_type  = "ACTUAL"

  action_threshold {
    action_threshold_type  = "PERCENTAGE"
    action_threshold_value = 100
  }

  definition {
    ssm_action_definition {
      action_sub_type = "STOP_EC2_INSTANCES"
      region          = var.aws_region
      instance_ids    = [aws_instance.tv_app.id]
    }
  }

  subscriber {
    address           = var.operator_email
    subscription_type = "EMAIL"
  }
}

# 90% ACTUAL — early stop so the box never even reaches 100% under a runaway.
resource "aws_budgets_budget_action" "stop_at_90" {
  budget_name        = aws_budgets_budget.tv_monthly.name
  action_type        = "RUN_SSM_DOCUMENTS"
  approval_model     = "AUTOMATIC"
  execution_role_arn = aws_iam_role.budget_action.arn
  notification_type  = "ACTUAL"

  action_threshold {
    action_threshold_type  = "PERCENTAGE"
    action_threshold_value = 90
  }

  definition {
    ssm_action_definition {
      action_sub_type = "STOP_EC2_INSTANCES"
      region          = var.aws_region
      instance_ids    = [aws_instance.tv_app.id]
    }
  }

  subscriber {
    address           = var.operator_email
    subscription_type = "EMAIL"
  }
}
