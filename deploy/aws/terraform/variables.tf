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
  description = "EC2 instance type. MUST be m8g.large per operator lock 2026-05-29 (Graviton4, 8 GiB, $0.06416/hr ap-south-1; see daily-universe-scope-expansion-2026-05-27.md §7 Quote 5, which supersedes the 2026-05-27 t4g.large + 2026-05-18 t4g.medium locks)."
  type        = string
  default     = "m8g.large"

  validation {
    condition     = var.instance_type == "m8g.large"
    error_message = "Instance type is pinned to m8g.large (Graviton4, 8 GiB) per operator lock 2026-05-29 (Quote 5). 8 GiB at 2 vCPU needs the m-family 4:1 ratio (c8g=4 GiB too small, r8g=16 GiB wasteful). This SUPERSEDES the 2026-05-27 t4g.large lock. See daily-universe-scope-expansion-2026-05-27.md section 7."
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

variable "enable_eip" {
  description = "Provision a 24/7 Elastic IP (static public IP). FLIPPED TO TRUE 2026-05-31 (operator approved 'Yes — enable it now'). The 2026-05-29 §7 Quote 5 assumption that 'the instance gets a fresh public IP on each stop/start' proved FALSE: after the manual t4g→m8g.large upgrade (stop/modify/start), the instance's ENI has auto-assign-public-IP OFF (console: 'Auto-assigned IP address: –'), so it had NO public IP and NO internet path at all — it could not reach AWS Systems Manager (Fleet Manager showed 0 managed nodes → deploy `InvalidInstanceId`) NOR Dhan. AWS cannot add an ephemeral public IP to an already-running instance; only an EIP can. So the EIP is now mandatory for the box to function, not optional. Cost ~₹300/mo; needed for live orders anyway (then register this EIP with Dhan; 7-day modify cooldown applies)."
  type        = bool
  default     = true
}

variable "ebs_gp3_size_gb" {
  description = "Root EBS volume size in GB. 30 per operator lock 2026-05-29 §7 Quote 6 — 30 GB hot window keeps the all-in bill ~₹2,058/mo; the partition manager auto-archives partitions >90d to the cheaper S3 cold bucket (~4x cheaper than EBS/GB), so EBS holds only hot data. gp3 grows online (no stop, no data loss) — raise this anytime the hot window needs more."
  type        = number
  default     = 30

  validation {
    condition     = var.ebs_gp3_size_gb >= 10 && var.ebs_gp3_size_gb <= 200
    error_message = "EBS is sized 10-200 GB. 30 GB default per operator lock 2026-05-29 (hot window + S3 cold-tier archival keeps bill ~₹2,058/mo). gp3 grows online beyond this if needed."
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
  description = "SSM parameter name where the Telegram bot token is stored. Defaults to the STAGING path because the app runs TV_ENVIRONMENT=staging during the 3-month no-orders data-pull, and the seed populates /tickvault/staging/* (the /tickvault/prod/* path does not exist yet -> ParameterNotFound in the webhook Lambda, 2026-05-30). Flip to /tickvault/prod/telegram/bot-token when going live (TV_ENVIRONMENT=prod)."
  type        = string
  default     = "/tickvault/staging/telegram/bot-token"
}

variable "telegram_chat_id_ssm_param" {
  description = "SSM parameter name where the Telegram chat ID (numeric) is stored. Defaults to the STAGING path (see telegram_bot_token_ssm_param). Flip to /tickvault/prod/telegram/chat-id when going live."
  type        = string
  default     = "/tickvault/staging/telegram/chat-id"
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

variable "operator_phone" {
  description = "Operator phone number in E.164 format (e.g. +919876543210) for the SNS SMS alert leg — the 3rd fan-out channel after Telegram + email. OPTIONAL: leave empty (\"\") to skip SMS (no subscription is created). Set via TF_VAR_operator_phone. India SMS via SNS may require moving the account out of the SMS sandbox + DLT sender-ID registration (operator/AWS-account concern, not code)."
  type        = string
  default     = ""

  validation {
    condition     = var.operator_phone == "" || can(regex("^\\+[1-9][0-9]{7,14}$", var.operator_phone))
    error_message = "operator_phone must be empty or E.164 format (e.g. +919876543210)."
  }
}
