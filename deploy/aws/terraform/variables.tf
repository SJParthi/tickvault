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
  description = "EC2 instance type. MUST be c7i.xlarge per the ₹5,000/mo budget cap."
  type        = string
  default     = "c7i.xlarge"

  validation {
    condition     = var.instance_type == "c7i.xlarge"
    error_message = "Instance type is pinned to c7i.xlarge. Larger instances blow the ₹5,000/mo budget — see aws-budget.md."
  }
}

variable "ami_id" {
  description = "Ubuntu 24.04 LTS AMI ID for ap-south-1. Operator pins this via `scripts/aws-get-ami.sh`."
  type        = string
  # Placeholder — operator replaces with the output of:
  #   aws ec2 describe-images \
  #     --region ap-south-1 \
  #     --owners 099720109477 \
  #     --filters 'Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*' \
  #     --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text
  default = "ami-placeholder-replace-me"
}

variable "ebs_gp3_size_gb" {
  description = "Root EBS volume size in GB. 100 per aws-budget.md hot-data tier."
  type        = number
  default     = 100

  validation {
    condition     = var.ebs_gp3_size_gb <= 100
    error_message = "EBS is capped at 100GB per the budget rule. Larger = S3 lifecycle tiering required first."
  }
}

variable "key_name" {
  description = "Name of the existing EC2 key pair for SSH. Operator creates via `aws ec2 create-key-pair`."
  type        = string
  default     = "dlt-prod-key"
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
  default     = "/dlt/prod/telegram/bot-token"
}

variable "dhan_access_token_ssm_param" {
  description = "SSM parameter name where the Dhan access token cache is stored."
  type        = string
  default     = "/dlt/prod/dhan/access-token"
}
