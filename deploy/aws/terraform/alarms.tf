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

# alarm "high_cpu" REMOVED 2026-11-11 (operator Quote 53, alarm review
# docs/audits/2026-11-alarm-review.md). The comments above are kept as history.

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

# alarm "ebs_write_latency" REMOVED 2026-11-11 (operator Quote 53, alarm review
# docs/audits/2026-11-alarm-review.md). The comments above are kept as history.

# ---------------------------------------------------------------------------
# 5. Average NetworkOut > 50 MiB per 60-second period for 5 periods
# NetworkOut is a byte count; with one sample per period, this is 50 MiB/min.
# ---------------------------------------------------------------------------

# alarm "network_out_high" REMOVED 2026-11-11 (operator Quote 53, alarm review
# docs/audits/2026-11-alarm-review.md). The comments above are kept as history.

# ---------------------------------------------------------------------------
# Outputs for operator visibility
# ---------------------------------------------------------------------------

output "cloudwatch_alarms" {
  description = "Names of the CloudWatch alarms declared in this file (2 since 2026-11-11: the operator Quote 53 alarm review removed the CPU, EBS-latency and network-out alarms; the two status checks stay because each carries an EC2 self-heal action). All publish to tv-alerts SNS."
  value = [
    aws_cloudwatch_metric_alarm.instance_status_check.alarm_name,
    aws_cloudwatch_metric_alarm.system_status_check.alarm_name,
  ]
}
