# S9: CloudWatch alarms that publish to tv-${env}-alerts SNS topic.
#
# The Grafana alerts in deploy/docker/grafana/provisioning/alerting/
# fire for in-app metrics (tv_*). These CloudWatch alarms fire for
# INFRASTRUCTURE signals — the machine / disk / network layer —
# which Grafana can't see because the app itself might be down.
#
# Alarm → SNS → operator Telegram bot subscription.
#
# 5 alarms to stay within the free tier (CloudWatch free tier = 10
# alarms/month per account).

# ---------------------------------------------------------------------------
# 1. EC2 Status Check failed — hypervisor-level issue, needs hardware move
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "instance_status_check" {
  alarm_name          = "tv-${var.environment}-instance-status-failed"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "StatusCheckFailed_Instance"
  namespace           = "AWS/EC2"
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  alarm_description   = "EC2 Status Check failed — instance-level fault. See DR runbook §5 (Instance unreachable)."
  treat_missing_data  = "breaching"

  dimensions = {
    InstanceId = aws_instance.tv_app.id
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# 2. EC2 System Status Check failed — AWS hardware/network-level issue
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "system_status_check" {
  alarm_name          = "tv-${var.environment}-system-status-failed"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "StatusCheckFailed_System"
  namespace           = "AWS/EC2"
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  alarm_description   = "AWS System Check failed — requires reboot to trigger underlying-host move. See DR runbook §5."
  treat_missing_data  = "breaching"

  dimensions = {
    InstanceId = aws_instance.tv_app.id
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# 3. CPU > 90% for 5 min — something is CPU-bound (might be normal during
# high-vol minutes, but sustained = investigate)
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "high_cpu" {
  alarm_name          = "tv-${var.environment}-cpu-high-5min"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 5
  metric_name         = "CPUUtilization"
  namespace           = "AWS/EC2"
  period              = 60
  statistic           = "Average"
  threshold           = 90
  alarm_description   = "CPU > 90% for 5 consecutive minutes. Investigate hot path allocation or trace a CPU flamegraph."
  treat_missing_data  = "notBreaching"

  dimensions = {
    InstanceId = aws_instance.tv_app.id
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
  ok_actions    = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# 4. EBS volume read/write latency > 50ms — disk slowness can break tick
# persistence. QuestDB ILP writes go through this volume.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "ebs_write_latency" {
  alarm_name          = "tv-${var.environment}-ebs-write-latency-high"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  metric_name         = "VolumeTotalWriteTime"
  namespace           = "AWS/EBS"
  period              = 300
  statistic           = "Average"
  threshold           = 50 # milliseconds
  alarm_description   = "EBS write latency > 50ms for 15 minutes. Tick spill + QuestDB ILP use this volume. See DR runbook §8 (Spill disk)."
  treat_missing_data  = "notBreaching"

  dimensions = {
    VolumeId = aws_instance.tv_app.root_block_device[0].volume_id
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# 5. Network out > 50 MB/sec for 5 min — possibly a runaway egress
# (tick replay loop, debug dump, or compromise)
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "network_out_high" {
  alarm_name          = "tv-${var.environment}-network-out-runaway"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 5
  metric_name         = "NetworkOut"
  namespace           = "AWS/EC2"
  period              = 60
  statistic           = "Average"
  threshold           = 52428800 # 50 MB/sec in bytes
  alarm_description   = "Network egress > 50 MB/sec for 5 minutes. Unusual — data transfer cost risk + possible runaway. See DR runbook §9."
  treat_missing_data  = "notBreaching"

  dimensions = {
    InstanceId = aws_instance.tv_app.id
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# Outputs for operator visibility
# ---------------------------------------------------------------------------

output "cloudwatch_alarms" {
  description = "Names of the 5 CloudWatch alarms. All publish to tv-alerts SNS."
  value = [
    aws_cloudwatch_metric_alarm.instance_status_check.alarm_name,
    aws_cloudwatch_metric_alarm.system_status_check.alarm_name,
    aws_cloudwatch_metric_alarm.high_cpu.alarm_name,
    aws_cloudwatch_metric_alarm.ebs_write_latency.alarm_name,
    aws_cloudwatch_metric_alarm.network_out_high.alarm_name,
  ]
}
