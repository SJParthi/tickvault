# S7-Step2 / Phase 8.1: DLT AWS stack — everything the ₹5,000/mo budget needs.
#
# Deployed resources:
#   - VPC with a single public subnet (no NAT to stay under budget)
#   - Security group: SSH from operator_cidr, no inbound from market
#   - IAM role: SSM read + CloudWatch write + EC2 metadata
#   - EC2 c7i.xlarge with gp3 100GB root volume
#   - Elastic IP (required for Dhan static IP mandate effective 2026-04-01)
#   - SSM parameters for Telegram bot token + Dhan access token
#   - SNS topic for CRITICAL alerts
#   - EventBridge rules for weekday 8am IST start, 5pm IST stop;
#     weekend 8am start, 1pm stop
#   - CloudWatch log group + metric alarms (5 core metrics)

# ---------------------------------------------------------------------------
# Networking
# ---------------------------------------------------------------------------

resource "aws_vpc" "dlt" {
  cidr_block           = "10.42.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = {
    Name = "dlt-${var.environment}-vpc"
  }
}

resource "aws_internet_gateway" "dlt" {
  vpc_id = aws_vpc.dlt.id

  tags = {
    Name = "dlt-${var.environment}-igw"
  }
}

resource "aws_subnet" "public" {
  vpc_id                  = aws_vpc.dlt.id
  cidr_block              = "10.42.1.0/24"
  availability_zone       = "${var.aws_region}a"
  map_public_ip_on_launch = false # we use a static EIP

  tags = {
    Name = "dlt-${var.environment}-public-a"
  }
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.dlt.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.dlt.id
  }

  tags = {
    Name = "dlt-${var.environment}-public-rt"
  }
}

resource "aws_route_table_association" "public" {
  subnet_id      = aws_subnet.public.id
  route_table_id = aws_route_table.public.id
}

# ---------------------------------------------------------------------------
# Security
# ---------------------------------------------------------------------------

resource "aws_security_group" "dlt_app" {
  name        = "dlt-${var.environment}-app"
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
    Name = "dlt-${var.environment}-app-sg"
  }
}

# ---------------------------------------------------------------------------
# IAM
# ---------------------------------------------------------------------------

resource "aws_iam_role" "dlt_instance" {
  name = "dlt-${var.environment}-instance-role"

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

resource "aws_iam_role_policy" "dlt_instance" {
  name = "dlt-${var.environment}-instance-policy"
  role = aws_iam_role.dlt_instance.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ssm:GetParameter",
          "ssm:GetParameters",
          "ssm:PutParameter",
        ]
        Resource = [
          "arn:aws:ssm:${var.aws_region}:*:parameter/dlt/${var.environment}/*"
        ]
      },
      {
        Effect = "Allow"
        Action = [
          "sns:Publish",
        ]
        Resource = aws_sns_topic.dlt_alerts.arn
      },
      {
        Effect = "Allow"
        Action = [
          "logs:CreateLogStream",
          "logs:PutLogEvents",
          "cloudwatch:PutMetricData",
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
          "arn:aws:s3:::dlt-${var.environment}-cold/*",
          "arn:aws:s3:::dlt-${var.environment}-cold"
        ]
      }
    ]
  })
}

resource "aws_iam_instance_profile" "dlt_instance" {
  name = "dlt-${var.environment}-instance-profile"
  role = aws_iam_role.dlt_instance.name
}

# ---------------------------------------------------------------------------
# EC2 instance + EIP
# ---------------------------------------------------------------------------

resource "aws_instance" "dlt_app" {
  ami                    = var.ami_id
  instance_type          = var.instance_type
  subnet_id              = aws_subnet.public.id
  vpc_security_group_ids = [aws_security_group.dlt_app.id]
  key_name               = var.key_name
  iam_instance_profile   = aws_iam_instance_profile.dlt_instance.name
  monitoring             = true

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required" # IMDSv2 mandatory
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type           = "gp3"
    volume_size           = var.ebs_gp3_size_gb
    iops                  = 3000
    throughput            = 125
    encrypted             = true
    delete_on_termination = false
    tags = {
      Name = "dlt-${var.environment}-root"
    }
  }

  user_data_replace_on_change = true
  user_data = templatefile("${path.module}/user-data.sh.tftpl", {
    environment = var.environment
    region      = var.aws_region
  })

  tags = {
    Name = "dlt-${var.environment}-app"
  }

  lifecycle {
    ignore_changes = [ami]
  }
}

resource "aws_eip" "dlt_app" {
  domain   = "vpc"
  instance = aws_instance.dlt_app.id

  tags = {
    Name = "dlt-${var.environment}-eip"
  }
}

# ---------------------------------------------------------------------------
# SNS topic for CRITICAL alerts
# ---------------------------------------------------------------------------

resource "aws_sns_topic" "dlt_alerts" {
  name = "dlt-${var.environment}-alerts"
}

# ---------------------------------------------------------------------------
# CloudWatch log group
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_log_group" "dlt_app" {
  name              = "/dlt/${var.environment}/app"
  retention_in_days = 14 # 14 days hot, S3 lifecycle handles cold
}

# ---------------------------------------------------------------------------
# EventBridge rules for the weekday/weekend start-stop schedule
# per aws-budget.md. Rules call SSM Automation to start/stop the instance.
#
# IST offset is UTC+5:30, so:
#   Weekday start 08:00 IST = 02:30 UTC (Mon-Fri)
#   Weekday stop  17:00 IST = 11:30 UTC (Mon-Fri)
#   Weekend start 08:00 IST = 02:30 UTC (Sat-Sun)
#   Weekend stop  13:00 IST = 07:30 UTC (Sat-Sun)
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_event_rule" "weekday_start" {
  name                = "dlt-${var.environment}-weekday-start"
  description         = "Start DLT instance at 08:00 IST on weekdays"
  schedule_expression = "cron(30 2 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_rule" "weekday_stop" {
  name                = "dlt-${var.environment}-weekday-stop"
  description         = "Stop DLT instance at 17:00 IST on weekdays"
  schedule_expression = "cron(30 11 ? * MON-FRI *)"
}

resource "aws_cloudwatch_event_rule" "weekend_start" {
  name                = "dlt-${var.environment}-weekend-start"
  description         = "Start DLT instance at 08:00 IST on weekends"
  schedule_expression = "cron(30 2 ? * SAT-SUN *)"
}

resource "aws_cloudwatch_event_rule" "weekend_stop" {
  name                = "dlt-${var.environment}-weekend-stop"
  description         = "Stop DLT instance at 13:00 IST on weekends"
  schedule_expression = "cron(30 7 ? * SAT-SUN *)"
}

# S3 cold-storage bucket for tick data > 14 days old.
resource "aws_s3_bucket" "dlt_cold" {
  bucket = "dlt-${var.environment}-cold"
}

resource "aws_s3_bucket_lifecycle_configuration" "dlt_cold" {
  bucket = aws_s3_bucket.dlt_cold.id

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

resource "aws_s3_bucket_public_access_block" "dlt_cold" {
  bucket                  = aws_s3_bucket.dlt_cold.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}
