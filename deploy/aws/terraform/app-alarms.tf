# App-level CloudWatch alarms — Z+ L2 VERIFY layer.
#
# The 5 alarms in alarms.tf cover infrastructure (EC2 status, CPU, EBS,
# network). These 12 alarms cover application signals: WebSocket health,
# QuestDB connectivity, token lifecycle, tick freshness, order rejection,
# aggregator liveness, backpressure, clock drift, composite SLO score.
#
# Charter authority: operator-charter-forever.md §C row "100% monitoring"
# + §F "Severity::Critical → Telegram". Without these the operator only
# learns about app failures by tailing /opt/tickvault/logs/errors.jsonl
# via SSM — which is reactive, not proactive.
#
# Data path:
#   tickvault Rust binary -> :9091/metrics (Prometheus exporter)
#                         -> CloudWatch agent prometheus scrape (60s interval)
#                         -> EMF processor filter (only the 12 metrics below)
#                         -> CloudWatch namespace "Tickvault/Prod"
#                         -> CloudWatch alarm
#                         -> SNS tv_alerts
#                         -> Telegram webhook Lambda (PR #781)
#                         -> Operator's phone
#
# Filter is configured in user-data.sh.tftpl::amazon-cloudwatch-agent.json
# emf_processor block — keeps custom-metric cost capped (12 metrics × $0.30
# = $3.60/mo ≈ ₹306/mo vs. ~₹4500/mo for an unfiltered 150-metric scrape).
#
# Cost honesty:
#   - CloudWatch free tier: 10 alarms + 10 custom metrics + 5GB logs.
#   - Pre-PR:  6 alarms (alarms.tf=5, telegram-webhook-lambda.tf=1). 0 custom metrics.
#   - Post-PR: 18 alarms, 12 custom metrics.
#   - Overage: 8 alarms × $0.10 = $0.80/mo + 2 custom metrics × $0.30 = $0.60/mo.
#   - Net: ~$1.40/mo ≈ ₹120/mo extra. Pushes aws-budget.md total from
#     ₹1,022 to ~₹1,142. Operator MUST acknowledge before terraform apply.

locals {
  # All alarms publish to the same SNS topic. Single source of truth so
  # the operator can swap actions topic-wide in one place.
  app_alarm_actions = [aws_sns_topic.tv_alerts.arn]
  app_alarm_ok      = [aws_sns_topic.tv_alerts.arn]
  app_namespace     = "Tickvault/Prod"
  app_dimensions    = { host = "tickvault-prod" }
}

# ---------------------------------------------------------------------------
# 1. Main-feed WebSocket pool dead — every conn dropped, no live ticks
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ws_pool_all_dead" {
  alarm_name          = "tv-${var.environment}-ws-pool-all-dead"
  alarm_description   = "Main-feed WebSocket pool reports all conns dead — no live ticks reaching the pipeline. See disaster-recovery.md scenario 6."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_websocket_pool_all_dead"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  treat_missing_data  = "breaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 2. WS pool partial degradation — some but not all conns failed
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ws_failed_connections" {
  alarm_name          = "tv-${var.environment}-ws-failed-connections"
  alarm_description   = "One or more main-feed conns are in failed state. Pool may self-heal via watchdog; investigate if sustained."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  metric_name         = "tv_websocket_failed_connections_count"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 3. Order-update WebSocket down — orders fly blind
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "order_update_ws_inactive" {
  alarm_name          = "tv-${var.environment}-order-update-ws-inactive"
  alarm_description   = "Order-update WebSocket is inactive. New orders will not receive fill confirmations via WS (postback fallback only)."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_order_update_ws_active"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Minimum"
  threshold           = 1
  treat_missing_data  = "breaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 4. QuestDB disconnected — persistence backed up to rescue ring + spill
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "questdb_disconnected" {
  alarm_name          = "tv-${var.environment}-questdb-disconnected"
  alarm_description   = "QuestDB has been disconnected for > 30 seconds. Ticks buffer in the 100K rescue ring. See BOOT-01/BOOT-02 runbook."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_questdb_disconnected_seconds"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 30
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 5. Many instruments silent — partial feed degradation
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "tick_gap_instruments_silent" {
  alarm_name          = "tv-${var.environment}-tick-gap-instruments-silent"
  alarm_description   = "> 100 instruments have been silent for the tick-gap threshold (30s default). Likely a slow socket or Dhan segment outage. See WS-GAP-06 runbook."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_tick_gap_instruments_silent"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 100
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 6. JWT token expiring within 4h — must force-renew before SEBI 24h cap
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "token_remaining_low" {
  alarm_name          = "tv-${var.environment}-token-remaining-low"
  alarm_description   = "Dhan JWT has < 4h remaining. Token manager should auto-refresh; alarm if it does not. See AUTH-GAP-03 runbook."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 3
  metric_name         = "tv_token_remaining_seconds"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Minimum"
  threshold           = 14400 # 4 hours
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 7. Spill ring dropping ticks — backpressure breach, downstream slow
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "spill_dropped" {
  alarm_name          = "tv-${var.environment}-spill-dropped"
  alarm_description   = "Spill writer is dropping ticks — rescue ring + spill both saturated. DLQ NDJSON catches them. See STORAGE-GAP-03."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_spill_dropped_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 8. DLQ catching ticks — last-resort sink in use
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "dlq_ticks" {
  alarm_name          = "tv-${var.environment}-dlq-ticks"
  alarm_description   = "Dead-letter queue is catching ticks — all upstream tiers (ring + spill) saturated. Investigate immediately."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_dlq_ticks_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 9. Aggregator producing zero seals during market hours
#
# Note: this alarm is NOT market-hours-gated at the CloudWatch level (CW
# alarms can't query Prometheus). It will fire during off-hours (15:30 IST
# onwards). Acceptable cost: ~16h/day of expected ALARM state, but those
# fires coalesce into the Telegram webhook's bucket and the operator
# learns to ignore them after 15:30 IST. Better than missing a mid-market
# aggregator death because we hardcoded a market-hours gate.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "aggregator_no_seals" {
  alarm_name          = "tv-${var.environment}-aggregator-no-seals"
  alarm_description   = "Aggregator emitted zero seals in the last 5 minutes. Expected during off-hours (15:30 IST onwards) — investigate ONLY during 09:15-15:30 IST."
  comparison_operator = "LessThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_aggregator_seals_emitted_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  # No ok_actions — would page on every off-hour transition.
}

# ---------------------------------------------------------------------------
# 10. Order rejections — OMS or Dhan-side issue
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "orders_rejected" {
  alarm_name          = "tv-${var.environment}-orders-rejected"
  alarm_description   = "One or more orders rejected in the last 5 minutes. Could be DH-905 (bad input), DH-906 (order error), or risk-gate denial."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_orders_rejected_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 11. Composite real-time guarantee score critical (< 0.80)
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "realtime_guarantee_critical" {
  alarm_name          = "tv-${var.environment}-realtime-guarantee-critical"
  alarm_description   = "Composite real-time guarantee score < 0.80 — at least one dimension (WS, QuestDB, tick freshness, token, spill, Phase 2) is severely degraded. See SLO-02 runbook."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_realtime_guarantee_score"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Minimum"
  threshold           = 0.80
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 12. Wall-clock skew > 1s — IST timestamp math at risk
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "clock_skew_high" {
  alarm_name          = "tv-${var.environment}-clock-skew-high"
  alarm_description   = "Wall-clock skew > 1s vs trusted source. IST timestamps may cross day boundaries. BOOT-03 fires at >2s. See BOOT-03 runbook."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_clock_skew_seconds"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# Output — operator-facing reminder + alarm list
# ---------------------------------------------------------------------------

output "app_cloudwatch_alarms" {
  description = "The 12 application-level alarms scraping Prometheus via the CloudWatch agent. Pre-merge cost note: pushes total alarms from 6 → 18 and adds 12 custom metrics. Overage above free tier: ~$1.40/mo ≈ ₹120/mo (aws-budget.md total ₹1,022 → ~₹1,142)."
  value = [
    aws_cloudwatch_metric_alarm.ws_pool_all_dead.alarm_name,
    aws_cloudwatch_metric_alarm.ws_failed_connections.alarm_name,
    aws_cloudwatch_metric_alarm.order_update_ws_inactive.alarm_name,
    aws_cloudwatch_metric_alarm.questdb_disconnected.alarm_name,
    aws_cloudwatch_metric_alarm.tick_gap_instruments_silent.alarm_name,
    aws_cloudwatch_metric_alarm.token_remaining_low.alarm_name,
    aws_cloudwatch_metric_alarm.spill_dropped.alarm_name,
    aws_cloudwatch_metric_alarm.dlq_ticks.alarm_name,
    aws_cloudwatch_metric_alarm.aggregator_no_seals.alarm_name,
    aws_cloudwatch_metric_alarm.orders_rejected.alarm_name,
    aws_cloudwatch_metric_alarm.realtime_guarantee_critical.alarm_name,
    aws_cloudwatch_metric_alarm.clock_skew_high.alarm_name,
  ]
}
