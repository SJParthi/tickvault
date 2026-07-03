# =============================================================================
# EventBridge → SSM start/stop dead-letter queue + retry observability
# =============================================================================
# The daily_start (08:30 IST) / daily_stop (16:30 IST) EventBridge targets in
# main.tf fire SSM AWS-StartEC2Instance / AWS-StopEC2Instance. Before this file
# those targets had NO retry policy and NO dead-letter queue, so a single
# transient SSM/EC2 throttle silently dropped the invocation with zero retries
# and nothing to inspect — the recurring "08:30 auto-start FAILED" incidents
# (Jun 08/11/15 2026) that forced the 08:45 watchdog + the operator to start the
# box by hand.
#
# This DLQ is the one the aws-startinstances-failed.md runbook already references
# (`tv-<env>-eventbridge-dlq` + "SQS DLQ depth alarm") but which never existed.
# Paired with the retry_policy + dead_letter_config added to the targets in
# main.tf, a transient hiccup now retries, and a persistent failure lands here
# and auto-pages — no human polling required.
# =============================================================================

resource "aws_sqs_queue" "eventbridge_dlq" {
  name = "tv-${var.environment}-eventbridge-dlq"
  # 3-day retention (B5, 2026-07-03): a bounded drain-after-inspect window.
  # Covers a Friday-evening failure through Monday inspection, then the queue
  # self-drains — the alarm below is edge-triggered on NEW arrivals
  # (NumberOfMessagesSent), so retention no longer controls how long a page
  # latches; it only bounds how long a dead-lettered payload stays inspectable.
  message_retention_seconds = 259200
  sqs_managed_sse_enabled   = true

  tags = {
    Name = "tv-${var.environment}-eventbridge-dlq"
  }
}

# Allow ONLY the EventBridge service, and ONLY our two schedule rules, to send
# failed invocations to the DLQ (least privilege via aws:SourceArn).
resource "aws_sqs_queue_policy" "eventbridge_dlq" {
  queue_url = aws_sqs_queue.eventbridge_dlq.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "AllowEventBridgeStartStopRulesToSendToDLQ"
        Effect    = "Allow"
        Principal = { Service = "events.amazonaws.com" }
        Action    = "sqs:SendMessage"
        Resource  = aws_sqs_queue.eventbridge_dlq.arn
        Condition = {
          ArnEquals = {
            "aws:SourceArn" = [
              aws_cloudwatch_event_rule.daily_start.arn,
              aws_cloudwatch_event_rule.daily_stop.arn,
            ]
          }
        }
      }
    ]
  })
}

# Auto-page when a NEW message lands in the DLQ — i.e. the EventBridge retry
# ladder was exhausted and a start/stop genuinely failed. This is the
# "no human input" backstop: a persistent failure now pages instead of being
# discovered by the operator at 09:05.
#
# SEMANTICS FIX (B5, 2026-07-03): the previous metric
# (ApproximateNumberOfMessagesVisible, Maximum > 0) LATCHED the alarm in
# ALARM state for the entire SQS retention window while a stale, already-
# inspected message sat in the queue — days of a permanently-red alarm with
# no new failure. NumberOfMessagesSent (Sum > 0 per 5-min period) fires the
# period a NEW message dead-letters, then self-clears; the queue self-drains
# after the bounded 3-day inspect window above. The resource + alarm names
# keep the historical "depth" wording deliberately — renaming would
# delete/recreate the alarm for zero benefit.
resource "aws_cloudwatch_metric_alarm" "eventbridge_dlq_depth" {
  alarm_name          = "tv-${var.environment}-eventbridge-dlq-depth"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "NumberOfMessagesSent"
  namespace           = "AWS/SQS"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  alarm_description   = "A NEW EC2 start/stop EventBridge invocation exhausted its retries and dead-lettered — the daily 08:30/16:30 IST schedule failed. Pages on new dead-letter arrival (edge-trigger, self-clears next period); the queue self-drains after the bounded 3-day inspect window. Inspect the DLQ message + CloudTrail StartInstances. See aws-startinstances-failed.md."
  treat_missing_data  = "notBreaching"

  dimensions = {
    QueueName = aws_sqs_queue.eventbridge_dlq.name
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = [aws_sns_topic.tv_alerts.arn]
}
