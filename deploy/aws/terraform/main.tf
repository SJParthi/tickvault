# DLT AWS stack — r8g.large budget envelope (~₹2,919/mo incl GST: 270 hrs,
# 30 GB EBS, +EIP kept; operator-lock 2026-06-30 in
# daily-universe-scope-expansion-2026-05-27.md §7 Quote 7, which supersedes
# the 2026-05-29 m8g.large + 2026-05-27 t4g.large + 2026-05-18 t4g.medium locks).
#
# Deployed resources:
#   - VPC with a single public subnet (no NAT to stay under budget)
#   - Security group: SSH from operator_cidr, no inbound from market
#   - IAM role: SSM read+write+delete (instance lock) + CloudWatch write +
#     SNS publish + S3 cold-tier read/write
#   - EC2 r8g.large (ARM Graviton4, 2 vCPU / 16 GiB) with gp3 30GB root volume
#   - Elastic IP — count-gated on var.enable_eip (DEFAULT false for the 3-month
#     data-pull: no orders → no Dhan static-IP whitelist need → ~₹430/mo saved.
#     Flip enable_eip=true before going LIVE with orders; 7-day modify cooldown)
#   - SSM parameters for Dhan credentials, Telegram tokens, QuestDB creds,
#     instance lock (dual-instance prevention — see crates/core/src/instance_lock.rs)
#   - SNS topic for CRITICAL alerts → 4-channel fan-out (SMS+Telegram+Email+Connect)
#   - EventBridge rules for daily 08:30 IST start / 16:30 IST stop (Mon-Fri
#     trading weekdays only per operator lock 2026-05-29 §7 Quote 5; weekends +
#     NSE holidays = OFF unless the operator manually starts the instance)
#   - CloudWatch log group + metric alarms (5 core infrastructure signals)
#
# IN-PLACE UPGRADE CONTRACT (operator lock 2026-06-30 §7 Quote 7): the running
# instance i-0b956d0209231a48b (tv-prod-app) is upgraded m8g.large → r8g.large by
# scripts/aws-upgrade-instance.sh (stop → modify-instance-attribute → start) at
# a controlled off-market time, NOT by `terraform apply`. Terraform therefore
# IGNORES instance_type + user_data on aws_instance.tv_app (lifecycle block
# below) so a merge-triggered apply can NEVER replace/wipe the box. Code deploys
# happen over SSM (git pull && docker compose up), never via instance replace.
#
# Stack components NOT deployed (CloudWatch-only migration #O1/#O2/#O3/#O4):
#   - Grafana / Prometheus / Alertmanager / Valkey — all retired.
#     Operator observability = QuestDB Console (local) + CloudWatch (prod).

# ---------------------------------------------------------------------------
# Networking
# ---------------------------------------------------------------------------

resource "aws_vpc" "dlt" {
  cidr_block           = "10.42.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = {
    Name = "tv-${var.environment}-vpc"
  }
}

resource "aws_internet_gateway" "dlt" {
  vpc_id = aws_vpc.dlt.id

  tags = {
    Name = "tv-${var.environment}-igw"
  }
}

resource "aws_subnet" "public" {
  vpc_id            = aws_vpc.dlt.id
  cidr_block        = "10.42.1.0/24"
  availability_zone = "${var.aws_region}a"
  # Auto-assign a public IP on launch so the instance is reachable for
  # the data-pull window WITHOUT a static EIP (var.enable_eip = false,
  # operator lock 2026-05-29 §7 Quote 5 — no orders, no Dhan static-IP
  # need). When enable_eip flips true for live trading, the EIP overrides
  # this with a stable address.
  map_public_ip_on_launch = true

  tags = {
    Name = "tv-${var.environment}-public-a"
  }
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.dlt.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.dlt.id
  }

  tags = {
    Name = "tv-${var.environment}-public-rt"
  }
}

resource "aws_route_table_association" "public" {
  subnet_id      = aws_subnet.public.id
  route_table_id = aws_route_table.public.id
}

# ---------------------------------------------------------------------------
# Security
# ---------------------------------------------------------------------------

resource "aws_security_group" "tv_app" {
  name        = "tv-${var.environment}-app"
  description = "DLT app: SSH from operator, egress to Dhan + AWS services"
  vpc_id      = aws_vpc.dlt.id

  ingress {
    description = "SSH from operator"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.operator_cidr]
  }

  egress {
    description = "All outbound (Dhan WS + REST + AWS services)"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Name = "tv-${var.environment}-app-sg"
  }
}

# ---------------------------------------------------------------------------
# IAM
# ---------------------------------------------------------------------------

resource "aws_iam_role" "tv_instance" {
  name = "tv-${var.environment}-instance-role"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Principal = {
        Service = "ec2.amazonaws.com"
      }
      Action = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "tv_instance" {
  name = "tv-${var.environment}-instance-policy"
  role = aws_iam_role.tv_instance.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ssm:GetParameter",
          "ssm:GetParameters",
          "ssm:PutParameter",
          # DeleteParameter needed for graceful release of the
          # dual-instance lock at shutdown — see PR #764
          # `crates/core/src/instance_lock.rs::release_instance_lock`.
          "ssm:DeleteParameter",
        ]
        Resource = [
          # Single real env (operator 2026-06-30): the app's TV_ENVIRONMENT=prod
          # SSM prefix (systemd unit) and var.environment (prod, names the infra
          # resources) AGREE, so one grant covers reading /tickvault/prod/*. The
          # retired /tickvault/staging/* grant was dropped with the dev/staging
          # consolidation. Least-privilege: only this one named env prefix, not
          # /tickvault/*. NO real orders — production.toml locks dry_run=true.
          "arn:aws:ssm:${var.aws_region}:*:parameter/tickvault/${var.environment}/*"
        ]
      },
      {
        Effect = "Allow"
        Action = [
          "sns:Publish",
        ]
        Resource = aws_sns_topic.tv_alerts.arn
      },
      {
        Effect = "Allow"
        Action = [
          # CreateLogGroup + DescribeLogStreams added 2026-06-12: the agent
          # needs them to (re)create the /tickvault/prod/app group + stream and
          # to verify existing streams; without them PutLogEvents can be denied,
          # leaving the log group empty (the symptom this change fixes).
          "logs:CreateLogGroup",
          "logs:CreateLogStream",
          "logs:DescribeLogStreams",
          "logs:PutLogEvents",
          "cloudwatch:PutMetricData",
          # The amazon-cloudwatch-agent calls ec2:DescribeTags in `-m ec2` mode to
          # resolve instance metadata for metric dimensions. Without it the agent
          # retried a 403 UnauthorizedOperation for ~60s then failed config
          # validation, starving the tickvault restart in the deploy script
          # (incident 2026-06-09, deploy #228 — observability fault took the live
          # trading app down). DescribeTags has no resource-level scoping, so "*".
          "ec2:DescribeTags",
        ]
        Resource = "*"
      },
      {
        Effect = "Allow"
        Action = [
          "s3:GetObject",
          "s3:PutObject",
          "s3:ListBucket",
        ]
        Resource = [
          "arn:aws:s3:::tv-${var.environment}-cold/*",
          "arn:aws:s3:::tv-${var.environment}-cold"
        ]
      },
      {
        # NSE-holiday self-stop: the boot-time holiday gate
        # (deploy/aws/holiday-gate.sh) stops THIS instance on a non-trading
        # day so the Mon-Fri start cron never bills a full no-op day.
        # Scoped by the Name tag (NOT the instance ARN) on purpose — the
        # instance -> instance-profile -> role -> policy chain would create a
        # dependency cycle if this referenced aws_instance.tv_app directly.
        # The tag condition still limits the action to exactly the tv-app box.
        Effect = "Allow"
        Action = [
          "ec2:StopInstances",
        ]
        Resource = "arn:aws:ec2:${var.aws_region}:*:instance/*"
        Condition = {
          StringEquals = {
            "ec2:ResourceTag/Name" = "tv-${var.environment}-app"
          }
        }
      }
    ]
  })
}

resource "aws_iam_instance_profile" "tv_instance" {
  name = "tv-${var.environment}-instance-profile"
  role = aws_iam_role.tv_instance.name
}

# SSM Managed-Instance Core — REQUIRED for the deploy-aws.yml workflow to run
# `aws ssm send-command` against this box (download binary -> smoke test ->
# atomic swap -> restart). The inline policy above grants ssm:GetParameter etc.
# for the APP to read its secrets, but the SSM *agent* needs a separate set of
# permissions (ssm:UpdateInstanceInformation, ssmmessages:*, ec2messages:*) to
# register the instance as a "managed instance". Without this attachment,
# send-command fails with `InvalidInstanceId: Instances not in a valid state
# for account` (observed in deploy-aws run #98, 2026-05-30). This AWS-managed
# policy is the standard, least-privilege way to enable SSM management +
# Session Manager (the operator's tunnel access in
# docs/runbooks/aws-access-from-anywhere.md also depends on it).
resource "aws_iam_role_policy_attachment" "tv_instance_ssm_core" {
  role       = aws_iam_role.tv_instance.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

# ---------------------------------------------------------------------------
# EC2 instance + EIP
# ---------------------------------------------------------------------------

resource "aws_instance" "tv_app" {
  ami                    = var.ami_id
  instance_type          = var.instance_type
  subnet_id              = aws_subnet.public.id
  vpc_security_group_ids = [aws_security_group.tv_app.id]
  key_name               = var.key_name
  iam_instance_profile   = aws_iam_instance_profile.tv_instance.name
  monitoring             = true

  # Terminate-protection ON (terminate destroys the EBS root volume + all
  # QuestDB data — the one truly irreversible action). To intentionally
  # `terraform destroy`, operator first runs:
  #   aws ec2 modify-instance-attribute --instance-id <id> --no-disable-api-termination
  # then re-applies with the flag false. Two-step destroy = the defense.
  #
  # Stop-protection OFF (operator lock 2026-05-29): the weekday 16:30 IST
  # EventBridge stop cron AND scripts/aws-upgrade-instance.sh BOTH need
  # ec2:StopInstances. `disable_api_stop = true` would silently block the
  # daily auto-stop → instance runs 24/7 → ~720 hrs/mo → ~₹5,500 bill instead
  # of the locked ~₹2,919/mo, and would block the in-place r8g.large upgrade.
  # Stop is reversible (EBS + data survive a stop) so it needs no guard.
  disable_api_termination = true
  disable_api_stop        = false

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required" # IMDSv2 mandatory
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type           = "gp3"
    volume_size           = var.ebs_gp3_size_gb
    iops                  = var.ebs_gp3_iops
    throughput            = var.ebs_gp3_throughput
    encrypted             = true
    delete_on_termination = false
    tags = {
      Name = "tv-${var.environment}-root"
    }
  }

  # user_data is the ONE-TIME bootstrap (installs Docker, clones repo, first
  # boot). After bootstrap, code deploys happen over SSM (git pull && docker
  # compose up) — NEVER by re-running user_data. So drift in this template must
  # NOT trigger an instance replace (replace = fresh root volume = QuestDB data
  # orphaned). false here + user_data in ignore_changes below = belt+suspenders.
  user_data_replace_on_change = false
  user_data = templatefile("${path.module}/user-data.sh.tftpl", {
    environment = var.environment
    region      = var.aws_region
  })

  tags = {
    Name = "tv-${var.environment}-app"
  }

  # NEVER let `terraform apply` replace or re-type the running box:
  #   - ami:           AMI refresh updates the default for NEW instances only;
  #                    existing instance keeps its AMI (no replace).
  #   - instance_type: the m8g.large → r8g.large upgrade is done out-of-band by
  #                    scripts/aws-upgrade-instance.sh at a controlled off-market
  #                    time. Terraform must not fight that or revert it. The
  #                    var.instance_type validation still documents the desired
  #                    r8g.large for any fresh provision.
  #   - user_data:     bootstrap-only (see note above); deploys are over SSM.
  #   - root_block_device size/iops/throughput: scripts/aws-upgrade-instance.sh
  #                    bumps these ONLINE (aws ec2 modify-volume, no stop, no data
  #                    loss). gp3 can grow but NOT shrink, so a later `terraform
  #                    apply` must NOT revert a script-bumped value back to the var
  #                    default (that would degrade QuestDB I/O, and a size revert is
  #                    impossible anyway). Ignoring them keeps the var as the
  #                    intent-for-fresh-provision while the live volume is owned by
  #                    the upgrade script.
  lifecycle {
    ignore_changes = [
      ami,
      instance_type,
      user_data,
      root_block_device[0].volume_size,
      root_block_device[0].iops,
      root_block_device[0].throughput,
    ]
  }
}

# Elastic IP — count-gated on var.enable_eip (DEFAULT 0 for the 3-month
# data-pull; no orders → no Dhan static-IP whitelist need → ~₹430/mo saved).
# Flip enable_eip=true before going LIVE with orders to provision + register
# a stable IP with Dhan. Reversible one-line change — matches the
# operator_phone count-gate pattern in sns-subscriptions.tf.
resource "aws_eip" "tv_app" {
  count    = var.enable_eip ? 1 : 0
  domain   = "vpc"
  instance = aws_instance.tv_app.id

  tags = {
    Name = "tv-${var.environment}-eip"
  }
}

# ---------------------------------------------------------------------------
# SNS topic for CRITICAL alerts
# ---------------------------------------------------------------------------

resource "aws_sns_topic" "tv_alerts" {
  name = "tv-${var.environment}-alerts"
}

# ---------------------------------------------------------------------------
# CloudWatch log group
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_log_group" "tv_app" {
  name              = "/tickvault/${var.environment}/app"
  retention_in_days = 14 # 14 days hot, S3 lifecycle handles cold
}

# ---------------------------------------------------------------------------
# EventBridge weekday start/stop schedule. Operator NARROWED the window back to
# 08:30-16:30 IST on 2026-06-05 (verbatim: "make the aws instance start and stop
# from 8.30 am till 4.30 pm") — supersedes the 2026-06-02 08:00-17:00 widening.
# Trading WEEKDAYS only (Mon-Fri); weekends + NSE holidays = instance OFF unless
# the operator manually starts it. Rules call SSM Automation to start/stop.
#
# Cost: -1 hr/day vs the 08:00-17:00 window (~-₹120/mo). The 08:30 start still
# gives the documented boot budget (§10) before the 09:00 pre-open.
#
# IST offset is UTC+5:30, so:
#   Weekday start 08:30 IST = 03:00 UTC (Mon-Fri)
#   Weekday stop  16:30 IST = 11:00 UTC (Mon-Fri)
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_event_rule" "daily_start" {
  name                = "tv-${var.environment}-daily-start"
  description         = "Start tickvault instance at 08:30 IST on trading weekdays (Mon-Fri)"
  schedule_expression = "cron(0 3 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_rule" "daily_stop" {
  name                = "tv-${var.environment}-daily-stop"
  description         = "Stop tickvault instance at 16:30 IST on trading weekdays (Mon-Fri)"
  schedule_expression = "cron(0 11 ? * MON-FRI *)"
}

# ---------------------------------------------------------------------------
# EventBridge → SSM Automation → EC2 start/stop targets
#
# Without targets the schedule rules above are dormant — they fire on cron
# but nothing happens. These resources wire each rule to AWS-managed SSM
# Automation documents that actually call ec2:StartInstances /
# ec2:StopInstances on our specific instance.
#
# IAM flow:
#   EventBridge (events.amazonaws.com)
#     → assumes `eventbridge_ec2_scheduler` role
#     → starts SSM Automation document `AWS-StartEC2Instance`
#     → SSM Automation uses the same role to call ec2:StartInstances
#
# The 2 rules (daily start + daily stop, Mon-Fri) share the same role —
# scoped to this one instance ARN.
# ---------------------------------------------------------------------------

data "aws_region" "current" {}
data "aws_caller_identity" "current" {}

resource "aws_iam_role" "eventbridge_ec2_scheduler" {
  name = "tv-${var.environment}-eventbridge-ec2-scheduler"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Principal = {
          Service = ["events.amazonaws.com", "ssm.amazonaws.com"]
        }
        Action = "sts:AssumeRole"
      }
    ]
  })
}

resource "aws_iam_role_policy" "eventbridge_ec2_scheduler" {
  name = "tv-${var.environment}-eventbridge-ec2-scheduler"
  role = aws_iam_role.eventbridge_ec2_scheduler.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ec2:StartInstances",
          "ec2:StopInstances",
          "ec2:DescribeInstances",
          "ec2:DescribeInstanceStatus"
        ]
        Resource = "*"
      },
      {
        Effect = "Allow"
        Action = [
          "ssm:StartAutomationExecution"
        ]
        Resource = [
          "arn:aws:ssm:${data.aws_region.current.name}::automation-definition/AWS-StartEC2Instance:*",
          "arn:aws:ssm:${data.aws_region.current.name}::automation-definition/AWS-StopEC2Instance:*"
        ]
      },
      {
        Effect   = "Allow"
        Action   = "iam:PassRole"
        Resource = "*"
        Condition = {
          StringEquals = {
            "iam:PassedToService" = "ssm.amazonaws.com"
          }
        }
      }
    ]
  })
}

locals {
  ssm_start_arn = "arn:aws:ssm:${data.aws_region.current.name}::automation-definition/AWS-StartEC2Instance:$DEFAULT"
  ssm_stop_arn  = "arn:aws:ssm:${data.aws_region.current.name}::automation-definition/AWS-StopEC2Instance:$DEFAULT"
}

resource "aws_cloudwatch_event_target" "daily_start" {
  rule      = aws_cloudwatch_event_rule.daily_start.name
  target_id = "start-instance"
  arn       = local.ssm_start_arn
  role_arn  = aws_iam_role.eventbridge_ec2_scheduler.arn

  input = jsonencode({
    InstanceId           = [aws_instance.tv_app.id]
    AutomationAssumeRole = [aws_iam_role.eventbridge_ec2_scheduler.arn]
  })

  # Self-heal a transient SSM/EC2 throttle at 08:30 IST instead of silently
  # dropping the start (the recurring "08:30 auto-start FAILED" incidents).
  # Retries within a 10-min window keep the box-up attempt going until well
  # before the 08:45 watchdog; an exhausted ladder dead-letters + alarms.
  retry_policy {
    maximum_retry_attempts       = 5
    maximum_event_age_in_seconds = 600
  }

  dead_letter_config {
    arn = aws_sqs_queue.eventbridge_dlq.arn
  }
}

resource "aws_cloudwatch_event_target" "daily_stop" {
  rule      = aws_cloudwatch_event_rule.daily_stop.name
  target_id = "stop-instance"
  arn       = local.ssm_stop_arn
  role_arn  = aws_iam_role.eventbridge_ec2_scheduler.arn

  input = jsonencode({
    InstanceId           = [aws_instance.tv_app.id]
    AutomationAssumeRole = [aws_iam_role.eventbridge_ec2_scheduler.arn]
  })

  # Same resilience on the 16:30 IST stop — a dropped stop means the box bills
  # 24/7 (~₹5,500/mo vs the locked ~₹2,919/mo per §7). Retry + DLQ
  # + the 16:45 stop-watchdog are the defense in depth.
  retry_policy {
    maximum_retry_attempts       = 5
    maximum_event_age_in_seconds = 600
  }

  dead_letter_config {
    arn = aws_sqs_queue.eventbridge_dlq.arn
  }
}

# S3 cold-storage bucket for tick data > 14 days old.
resource "aws_s3_bucket" "tv_cold" {
  bucket = "tv-${var.environment}-cold"
}

resource "aws_s3_bucket_lifecycle_configuration" "tv_cold" {
  bucket = aws_s3_bucket.tv_cold.id

  rule {
    id     = "tick-cold-tiering"
    status = "Enabled"

    filter {}

    transition {
      days          = 30
      storage_class = "INTELLIGENT_TIERING"
    }

    transition {
      days          = 365
      storage_class = "GLACIER_IR"
    }

    expiration {
      days = 1825 # 5 years per SEBI retention
    }
  }
}

resource "aws_s3_bucket_public_access_block" "tv_cold" {
  bucket                  = aws_s3_bucket.tv_cold.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}
