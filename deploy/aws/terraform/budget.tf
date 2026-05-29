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
# GST). Limit set to $25 for headroom (manual runs toward ~300 hrs + a
# little EBS growth) so it does not false-trip. Monitoring/Lambda/Telegram
# fan-out are free-tier (₹0). If INR/USD swings >10%, EBS grows, or the hour
# ceiling changes, the operator updates limit_amount manually.

resource "aws_budgets_budget" "tv_monthly" {
  name              = "tv-${var.environment}-monthly-budget"
  budget_type       = "COST"
  limit_amount      = "25"
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
