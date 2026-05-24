# Monthly cost budget alarm — fires at 80% forecasted + 100% actual.
#
# Charter authority: .claude/rules/project/aws-budget.md — t4g.medium
# locked at ~₹1,022/mo. The 80% forecasted alarm fires early enough that
# operator can take action (e.g., terminate a rogue stress test) before
# the bill arrives. The 100% actual alarm is the hard "we crossed the
# budget" signal.
#
# USD vs INR: AWS Budgets uses USD internally. ₹1,000 ≈ $12 USD (operator
# spot rate 2026-05-24, $1 ≈ ₹85). If INR/USD swings >10%, operator
# updates the limit_amount manually.

resource "aws_budgets_budget" "tv_monthly" {
  name              = "tv-${var.environment}-monthly-budget"
  budget_type       = "COST"
  limit_amount      = "12"
  limit_unit        = "USD"
  time_unit         = "MONTHLY"
  time_period_start = "2026-05-01_00:00"

  cost_filter {
    name   = "TagKeyValue"
    values = ["Project$tickvault"]
  }

  # 80% forecasted = early warning
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = [var.operator_email]
  }

  # 100% actual = hard crossing
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.operator_email]
  }
}

output "monthly_budget_name" {
  description = "AWS Budget name — visible at Billing > Budgets"
  value       = aws_budgets_budget.tv_monthly.name
}
