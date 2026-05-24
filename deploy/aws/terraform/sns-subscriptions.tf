# SNS subscriptions for tv-alerts topic.
#
# Charter authority: operator-charter-forever.md §C row "100% alerting" +
# aws-budget.md §6 — 4-channel fan-out (SMS + Telegram + Email + Connect).
# This file ships ONLY the email subscription on day 1; SMS / Telegram
# webhook / Connect outbound call land in follow-up PRs as the operator
# confirms the phone number and Telegram bot setup is ready.
#
# Email subscription confirmation flow:
#   1. terraform apply creates the subscription in PendingConfirmation state
#   2. AWS sends a confirmation email to var.operator_email
#   3. Operator clicks the link in the email (one-time, ~5 seconds)
#   4. Subscription transitions to Confirmed; CloudWatch alarms now route
#
# If the operator misses the confirmation email, alarms fire silently
# (logged in CloudWatch only). Mitigation: AWS Console > SNS > tv-alerts
# > Subscriptions shows the PendingConfirmation status; click "Request
# Confirmation" to resend.

resource "aws_sns_topic_subscription" "operator_email" {
  topic_arn = aws_sns_topic.tv_alerts.arn
  protocol  = "email"
  endpoint  = var.operator_email
}

# TODO (follow-up PR):
# - aws_sns_topic_subscription.operator_sms — protocol = "sms", endpoint = var.operator_phone
# - aws_sns_topic_subscription.telegram_webhook — protocol = "lambda", endpoint = aws_lambda_function.telegram_relay.arn
# - aws_connect_outbound_voice_contact — for Critical-severity wake-up call

output "operator_email_subscription_status" {
  description = "Reminder: check your inbox for the SNS confirmation email and click the link to activate alarm delivery."
  value       = "Subscription created for ${var.operator_email} — confirm via the email AWS just sent."
}
