# S7-Step2 / Phase 8.1: Input variables for the DLT AWS stack.
#
# Defaults match .claude/rules/project/aws-budget.md — the ₹5,000/mo cap.

variable "aws_region" {
  description = "AWS region. MUST be ap-south-1 (Mumbai) for low-latency Dhan access."
  type        = string
  default     = "ap-south-1"

  validation {
    condition     = var.aws_region == "ap-south-1"
    error_message = "DLT is pinned to ap-south-1 (Mumbai). Static IP has 7-day cooldown per Dhan — do NOT region-shop."
  }
}

variable "environment" {
  description = "Deployment environment: prod | staging"
  type        = string
  default     = "prod"

  validation {
    condition     = contains(["prod", "staging"], var.environment)
    error_message = "environment must be prod or staging"
  }
}

variable "instance_type" {
  description = "EC2 instance type. MUST be t4g.medium per operator lock 2026-05-18 (~₹1,022/mo, see aws-budget.md)."
  type        = string
  default     = "t4g.medium"

  validation {
    condition     = var.instance_type == "t4g.medium"
    error_message = "Instance type is pinned to t4g.medium per operator lock 2026-05-18. The 4-SID IDX_I universe + CloudWatch-only stack fits in 4 GiB. See aws-budget.md."
  }
}

variable "ami_id" {
  description = "Amazon Linux 2023 arm64 AMI for ap-south-1. t4g.medium is Graviton — arm64 is mandatory (x86_64 will fail to boot). AL2023 chosen 2026-05-24 over Ubuntu because CloudWatch agent + SSM agent + AWS CLI are pre-installed (no apt-get equivalents needed in user-data)."
  type        = string
  # Default = al2023-ami-2023.11.20260514.0 arm64 (operator confirmed via AWS
  # console 2026-05-24 — published 2026-05-15). Quarterly refresh recommended:
  #   aws ec2 describe-images \
  #     --region ap-south-1 \
  #     --owners amazon \
  #     --filters 'Name=name,Values=al2023-ami-2023.*-arm64' \
  #               'Name=virtualization-type,Values=hvm' \
  #     --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text
  # `aws_instance.tv_app.lifecycle.ignore_changes = [ami]` prevents drift
  # from refresh — existing instances keep their AMI; only new instances
  # pick up the latest default.
  default = "ami-0fa0340d4a8bdd6ee"

  validation {
    condition     = can(regex("^ami-[0-9a-f]{8,17}$", var.ami_id))
    error_message = "ami_id must be a valid AMI ID (format: ami-XXXXXXXXXXXXXXXXX). Run the aws ec2 describe-images command in the comment above to fetch the latest AL2023 arm64 AMI for ap-south-1."
  }
}

variable "ebs_gp3_size_gb" {
  description = "Root EBS volume size in GB. 10 per aws-budget.md (4-SID IDX_I dataset is tiny; partition manager prunes to S3)."
  type        = number
  default     = 10

  validation {
    condition     = var.ebs_gp3_size_gb >= 10 && var.ebs_gp3_size_gb <= 30
    error_message = "EBS is sized 10-30 GB for the 4-SID dataset per aws-budget.md operator-lock 2026-05-18. Larger needs S3 lifecycle tiering first."
  }
}

variable "key_name" {
  description = "Name of the existing EC2 key pair for SSH. Operator creates via `aws ec2 create-key-pair`."
  type        = string
  default     = "tv-prod-key"
}

variable "operator_cidr" {
  description = "CIDR that may SSH into the instance. Tighten to your home/office IP."
  type        = string
  default     = "0.0.0.0/0"

  validation {
    condition     = length(var.operator_cidr) > 0
    error_message = "operator_cidr must be a non-empty CIDR (e.g. 203.0.113.42/32)"
  }
}

variable "telegram_bot_token_ssm_param" {
  description = "SSM parameter name where the Telegram bot token is stored."
  type        = string
  default     = "/tickvault/prod/telegram/bot-token"
}

variable "telegram_chat_id_ssm_param" {
  description = "SSM parameter name where the Telegram chat ID (numeric) is stored."
  type        = string
  default     = "/tickvault/prod/telegram/chat-id"
}

variable "dhan_access_token_ssm_param" {
  description = "SSM parameter name where the Dhan access token cache is stored."
  type        = string
  default     = "/tickvault/prod/dhan/access-token"
}

variable "operator_email" {
  description = "Operator email address for CloudWatch alarm + budget notifications. SNS sends a confirmation link on first apply; operator clicks it once to activate. Required — no sensible default."
  type        = string

  validation {
    condition     = can(regex("^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Za-z]{2,}$", var.operator_email))
    error_message = "operator_email must be a valid email address (set via TF_VAR_operator_email=you@example.com)."
  }
}
