# CloudWatch infrastructure alarms — publish to tv-${env}-alerts SNS topic.
#
# Post-CloudWatch-only-migration (#O1/#O2/#O3/#O4): the entire observability
# stack is CloudWatch. App-level metrics (tv_*) ship via the CloudWatch agent;
# infrastructure signals (CPU/disk/network/instance status) ship via these
# AWS/EC2 namespace alarms. Operator sees both in one place: CloudWatch Console.
#
# Alarm → SNS → operator Telegram bot + SMS + Email + Connect call (4-channel
# fan-out per operator-charter-forever.md §F).
#
# 5 alarms to stay within the free tier (CloudWatch free tier = 10
# alarms/month per account).

# ---------------------------------------------------------------------------
# 1. EC2 Status Check failed — hypervisor-level issue, needs hardware move
# ---------------------------------------------------------------------------

# treat_missing_data = notBreaching (was "breaching", 2026-06-02): the box is
# auto-stopped daily at 17:00 IST (08:00–17:00 schedule). A STOPPED instance
# emits no StatusCheckFailed datapoints, so "breaching" flipped this alarm to a
# false ALARM every evening (pager fatigue). notBreaching keeps it OK while
# stopped, yet still fires on a real breaching datapoint (instance RUNNING but
# status check failing). Matches the app-alarms.tf fix pattern.
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
  treat_missing_data  = "notBreaching"

  dimensions = {
    InstanceId = aws_instance.tv_app.id
  }

  # BP-14 (audit 2026-07-01): a failed INSTANCE status check (hung OS / stuck
  # instance) is best cleared by a reboot, so add the EC2 auto-`reboot` action
  # ALONGSIDE the SNS page — the box self-heals a soft hang during market
  # hours instead of only paging. autopilot only handles a cleanly-stopped
  # box, not a status-impaired running one.
  alarm_actions = [
    aws_sns_topic.tv_alerts.arn,
    "arn:aws:automate:${var.aws_region}:ec2:reboot",
  ]
  ok_actions = [aws_sns_topic.tv_alerts.arn]
}

# ---------------------------------------------------------------------------
# 2. EC2 System Status Check failed — AWS hardware/network-level issue
# ---------------------------------------------------------------------------

# treat_missing_data = notBreaching (was "breaching", 2026-06-02): same as the
# instance-status alarm above — a STOPPED instance (daily 17:00 IST schedule)
# emits no datapoints, so "breaching" false-fired this alarm every evening.
# notBreaching stays OK while stopped, still fires on a real failed check.
resource "aws_cloudwatch_metric_alarm" "system_status_check" {
  alarm_name          = "tv-${var.environment}-system-status-failed"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "StatusCheckFailed_System"
  namespace           = "AWS/EC2"
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  alarm_description   = "AWS System Check failed — requires recover to trigger underlying-host move. See DR runbook §5."
  treat_missing_data  = "notBreaching"

  dimensions = {
    InstanceId = aws_instance.tv_app.id
  }

  # BP-14 (audit 2026-07-01): a failed SYSTEM status check is an AWS
  # hardware/host fault — the correct auto-remediation is EC2 `recover`, which
  # migrates the instance to healthy underlying hardware (EBS-backed, same
  # instance-id, same private IP + EIP). Add it ALONGSIDE the SNS page so a
  # hardware fault during market hours self-migrates instead of only paging.
  alarm_actions = [
    aws_sns_topic.tv_alerts.arn,
    "arn:aws:automate:${var.aws_region}:ec2:recover",
  ]
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
# 4. EBS per-operation write latency > 50ms — disk slowness can break tick
# persistence. QuestDB ILP writes go through this volume.
#
# SEMANTICS FIX (B5, 2026-07-03): `VolumeTotalWriteTime` is the CUMULATIVE
# SECONDS spent on writes in the period — NOT a millisecond latency. Alarming
# `Average(VolumeTotalWriteTime) > 50` compared seconds-per-period against a
# ms threshold and could never fire meaningfully. The correct per-op latency
# is metric math: Sum(write time) / Sum(write ops) × 1000 = avg ms per write.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "ebs_write_latency" {
  alarm_name          = "tv-${var.environment}-ebs-write-latency-high"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  threshold           = 50 # milliseconds per write operation (metric-math e1)
  alarm_description   = "EBS avg per-op write latency > 50ms for 15 minutes (metric math: Sum VolumeTotalWriteTime / Sum VolumeWriteOps × 1000). Tick spill + QuestDB ILP use this volume. See DR runbook §8 (Spill disk)."
  treat_missing_data  = "notBreaching"

  metric_query {
    id          = "e1"
    expression  = "IF(ops > 0, (wt / ops) * 1000, 0)"
    label       = "Avg ms per EBS write op"
    return_data = true
  }

  metric_query {
    id = "wt"

    metric {
      metric_name = "VolumeTotalWriteTime"
      namespace   = "AWS/EBS"
      period      = 300
      stat        = "Sum"

      dimensions = {
        VolumeId = aws_instance.tv_app.root_block_device[0].volume_id
      }
    }
  }

  metric_query {
    id = "ops"

    metric {
      metric_name = "VolumeWriteOps"
      namespace   = "AWS/EBS"
      period      = 300
      stat        = "Sum"

      dimensions = {
        VolumeId = aws_instance.tv_app.root_block_device[0].volume_id
      }
    }
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
