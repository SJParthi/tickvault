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
  description = "EC2 instance type. MUST be t4g.medium per operator lock 2026-07-15 (Graviton2 burstable, 2 vCPU / 4 GiB, $0.0224/hr ap-south-1; see daily-universe-scope-expansion-2026-05-27.md §7 Quote 8, which supersedes the 2026-06-30 r8g.large + 2026-05-29 m8g.large + 2026-05-27 t4g.large locks)."
  type        = string
  default     = "t4g.medium"

  validation {
    condition     = var.instance_type == "t4g.medium"
    error_message = "Instance type is pinned to t4g.medium (Graviton2, 4 GiB) per operator lock 2026-07-15 (Quote 8 downsize — the Groww-only runtime no longer needs the 16 GiB memory-optimized host; QuestDB is re-capped at 1g in lockstep). This SUPERSEDES the 2026-06-30 lock. See daily-universe-scope-expansion-2026-05-27.md section 7."
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
  description = "Root EBS volume size in GB. 20 per the 2026-07-15 downsize pre-stage (executor decision recorded in daily-universe-scope-expansion-2026-05-27.md §0 under Quote 8 + §7 Rule 3 — NOT operator-quoted scope): gp3 can NEVER shrink (`modify-volume` grows only, and a 50 GB snapshot cannot restore into a 20 GB volume), so the LIVE root stays 50 GB until a deliberate terminate-and-recreate in the operator's post-market data-erase window replaces it (the box is fully cattle-provisioned by user-data.sh.tftpl; the pre-downsize snapshot is the rollback). History: 10 -> 30 (2026-05-29 Quote 6) -> 50 (2026-07-13 disk-pressure grow) -> 20 target (2026-07-15). The partition manager archives partitions >90d to the cheaper S3 cold bucket, so 20 GB holds the hot window on the erased fresh volume. root_block_device[0].volume_size is in the instance lifecycle.ignore_changes so a `terraform apply` does NOT touch the LIVE volume. This var documents the intended size for a FRESH provision only; any LIVE grow stays out-of-band via scripts/aws-upgrade-instance.sh --ebs-size (online aws ec2 modify-volume, no stop)."
  type        = number
  default     = 20

  validation {
    condition     = var.ebs_gp3_size_gb >= 10 && var.ebs_gp3_size_gb <= 200
    error_message = "EBS is sized 10-200 GB. 20 GB default per the 2026-07-15 downsize pre-stage (fresh-volume replacement target; the live root stays 50 GB — gp3 cannot shrink; was 50 per the 2026-07-13 grow). gp3 grows online beyond this if needed."
  }
}

variable "ebs_gp3_iops" {
  description = "Root gp3 EBS provisioned IOPS. 3000 is the gp3 baseline (free, included). Range 3000-16000 — raise alongside throughput when the QuestDB write/read load grows (e.g. both feeds at ~2K SIDs). scripts/aws-upgrade-instance.sh can bump this online (no stop) via aws ec2 modify-volume; root_block_device[0].iops is in the instance lifecycle.ignore_changes so a later `terraform apply` does NOT revert a script-bumped value. This var documents the intended IOPS for a FRESH provision."
  type        = number
  default     = 3000

  validation {
    condition     = var.ebs_gp3_iops >= 3000 && var.ebs_gp3_iops <= 16000
    error_message = "ebs_gp3_iops must be 3000-16000 (gp3 range; 3000 is the free baseline)."
  }
}

variable "ebs_gp3_throughput" {
  description = "Root gp3 EBS throughput in MiB/s. 125 is the gp3 baseline (free, included). Range 125-1000 — raise alongside IOPS for heavier QuestDB I/O. scripts/aws-upgrade-instance.sh can bump this online (no stop) via aws ec2 modify-volume; root_block_device[0].throughput is in the instance lifecycle.ignore_changes so a later `terraform apply` does NOT revert a script-bumped value. This var documents the intended throughput for a FRESH provision."
  type        = number
  default     = 125

  validation {
    condition     = var.ebs_gp3_throughput >= 125 && var.ebs_gp3_throughput <= 1000
    error_message = "ebs_gp3_throughput must be 125-1000 MiB/s (gp3 range; 125 is the free baseline)."
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

variable "enable_questdb_console" {
  description = "Deploy the B4 QuestDB one-click console: API-GW → front Lambda (device-key auth + read-only SQL gate) → VPC proxy Lambda → box:9000 via SG-to-SG only (port 9000 never public). Reuses SSM SecureString /tickvault/<env>/operator/control-secret for auth. Mirrors enable_operator_control_lambda: default false; CI opts prod in via TF_VAR_enable_questdb_console."
  type        = bool
  default     = false
}

variable "telegram_bot_token_ssm_param" {
  description = "SSM parameter name where the Telegram bot token is stored. Defaults to /tickvault/prod/telegram/bot-token: the single real env is prod (TV_ENVIRONMENT=prod, operator 2026-06-30 — dev/staging retired), and the operator-populated prod params already EXIST while the old /tickvault/staging/* path is now EMPTY (the stale staging default would 404 ParameterNotFound in the webhook Lambda)."
  type        = string
  default     = "/tickvault/prod/telegram/bot-token"
}

variable "telegram_chat_id_ssm_param" {
  description = "SSM parameter name where the Telegram chat ID (numeric) is stored. Defaults to /tickvault/prod/telegram/chat-id (single real prod env; see telegram_bot_token_ssm_param)."
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

variable "operator_phone" {
  description = "Operator phone number in E.164 format (e.g. +919876543210) for the SNS SMS alert leg — the 3rd fan-out channel after Telegram + email. OPTIONAL: leave empty (\"\") to skip SMS (no subscription is created). Set via TF_VAR_operator_phone. India SMS via SNS may require moving the account out of the SMS sandbox + DLT sender-ID registration (operator/AWS-account concern, not code)."
  type        = string
  default     = ""

  validation {
    condition     = var.operator_phone == "" || can(regex("^\\+[1-9][0-9]{7,14}$", var.operator_phone))
    error_message = "operator_phone must be empty or E.164 format (e.g. +919876543210)."
  }
}

variable "portal_git_sha" {
  description = "Git SHA of the repo tree terraform/lambda zips were applied from (B9 deploy provenance — set by CI via TF_VAR_portal_git_sha=github.sha; local applies default to \"unknown\"). Surfaces in the operator-portal footer as `portal <sha7>` and in the portal_git_sha output."
  type        = string
  default     = "unknown"
}

variable "daily_loss_alarm_inr" {
  # = config/base.toml [risk] max_daily_loss_percent (2.0) × capital (1000000.0) = ₹20,000. Two sources of truth by necessity (terraform cannot read TOML); update BOTH together.
  description = "Daily loss alarm threshold in INR (positive number; the daily-loss-breach alarm fires when tv_daily_pnl Minimum < -1 x this). Lockstep with config/base.toml [risk] max_daily_loss_percent x capital."
  type        = number
  default     = 20000
}
