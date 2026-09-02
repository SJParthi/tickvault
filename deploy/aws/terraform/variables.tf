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
  description = "EC2 instance type. MUST be r8g.xlarge per operator lock 2026-08-08 (Graviton4 memory-optimised, 4 vCPU / 32 GiB; see daily-universe-scope-expansion-2026-05-27.md §7 Quote 13, which supersedes the 2026-08-07 t4g.large + 2026-07-15 t4g.medium + 2026-06-30 r8g.large + 2026-05-29 m8g.large locks). Sized for the 13-timeframe (1s/5s/10s/15s/30s + 1m/2m/3m/5m/15m/30m/60m + 1d) current-day workload WITH raw-tick retention at ~25,000 instruments: 13 TF x 128 B LiveCandleState x 25k = 42 MB, seal ring 29 MB, a day of ticks 2.3-7.2 GB, QuestDB 8-16 GB, app+OS 6-12 GB => 14-31 GB in 32 GiB. `r` (8 GiB/vCPU) because the workload is memory-bound: m8g would force buying unused CPU to reach the same RAM, r8gd's local NVMe is WIPED on every stop (the box stops daily), and r8i would force an x86 rebuild of the whole ARM pipeline. NOTE the AZ pin was removed in the same change — see var.availability_zone; the 2026-08-07 type-only flip failed with InsufficientInstanceCapacity precisely because it left the pin in place."
  type        = string
  default     = "r8g.xlarge"

  validation {
    condition     = var.instance_type == "r8g.xlarge"
    error_message = "Instance type is pinned to r8g.xlarge (Graviton4, 4 vCPU / 32 GiB) per operator lock 2026-08-08 (Quote 13 — the 13-timeframe + current-day tick-retention requirement). This SUPERSEDES the 2026-08-07 t4g.large lock. See daily-universe-scope-expansion-2026-05-27.md section 7."
  }
}

variable "availability_zone" {
  description = "Which ap-south-1 AZ suffix the instance launches into (a|b|c). Added 2026-08-08 (operator Quote 13) to END the single-AZ pin that kept the box unstartable 2026-08-06 -> 2026-08-08: ap-south-1a ran out of capacity, and a stopped instance can only restart in its own AZ, so every start returned InsufficientInstanceCapacity — and the 2026-08-07 escape attempt via a bigger instance type was refused for the SAME reason, proving the zone was the constraint. Subnets now exist in all three AZs (main.tf), so a capacity refusal is a one-variable change + re-apply instead of days of downtime. Default 'b' because 'a' is the zone that failed. NOTE: changing this REPLACES the instance (an instance cannot move zones) and the root EBS volume does NOT follow (EBS is zone-locked) — migrate via snapshot; see .claude/plans/proposals/2026-08-08-r8g-xlarge-migration.md Phase 2."
  type        = string
  default     = "b"

  validation {
    condition     = contains(["a", "b", "c"], var.availability_zone)
    error_message = "availability_zone must be one of a, b, c (the ap-south-1 AZ suffixes). All candidate instance types are offered in all three — verified 2026-08-08 via describe-instance-type-offerings."
  }
}

variable "ami_id" {
  description = "Amazon Linux 2023 arm64 AMI for ap-south-1. r8g.xlarge is Graviton4 — arm64 is mandatory (x86_64 will fail to boot), and that constraint is exactly why r8i was REJECTED in the Quote 13 sizing: an Intel type would force an x86 rebuild of the whole ARM pipeline including the lambdas. (Named t4g.medium here until 2026-08-12; corrected under operator Quote 15. The arm64 requirement is unchanged — both are Graviton — but a stale type name in an AMI description is how someone talks themselves into an x86 AMI.) AL2023 chosen 2026-05-24 over Ubuntu because CloudWatch agent + SSM agent + AWS CLI are pre-installed (no apt-get equivalents needed in user-data)."
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
  description = "Root EBS volume size in GB. 20 per the 2026-07-15 downsize pre-stage (executor decision recorded in daily-universe-scope-expansion-2026-05-27.md §0 under Quote 8 + §7 Rule 3 — NOT operator-quoted scope): gp3 can NEVER shrink (`modify-volume` grows only, and a larger snapshot cannot restore into a smaller volume), so the LIVE root stays at its current size until a deliberate terminate-and-recreate in the operator's post-market data-erase window replaces it (the box is fully cattle-provisioned by user-data.sh.tftpl; the pre-downsize snapshot is the rollback). LIVE SIZE CORRECTED 2026-07-19: describe-volumes on vol-073ccaa417a0f344b returned 30 GiB gp3 — the 2026-07-13 approved 30->50 grow was recorded but never physically applied (see daily-universe §7 2026-07-19 correction note). SAME-DAY RULING (daily-universe §0 Quote 9): 30 GB formally ACCEPTED, the grow CANCELLED — any future grow needs a fresh dated quote; the 20 GB fresh-provision target below stays a separate un-quoted executor pre-stage. History: 10 -> 30 (2026-05-29 Quote 6) -> [50 approved 2026-07-13, never applied; live verified 30 on 2026-07-19] -> 20 target (2026-07-15). The partition manager archives partitions >90d to the cheaper S3 cold bucket, so 20 GB holds the hot window on the erased fresh volume. root_block_device[0].volume_size is in the instance lifecycle.ignore_changes so a `terraform apply` does NOT touch the LIVE volume. This var documents the intended size for a FRESH provision only; any LIVE grow stays out-of-band via scripts/aws-upgrade-instance.sh --ebs-size (online aws ec2 modify-volume, no stop)."
  type        = number
  default     = 500

  validation {
    condition     = var.ebs_gp3_size_gb >= 10 && var.ebs_gp3_size_gb <= 500
    error_message = "EBS is sized 10-500 GB. 500 GB default per operator lock 2026-09-02 (daily-universe-scope-expansion-2026-05-27.md Quote 20 - operator asked \"isnatnce upgrade or disk upgrade needed?\" then authorized \"whatevr is needed and recommended go ahead dude okay? i just need the workign finalsied solution dude okay?\" against a reply naming the grow, its +$18.24/mo price, and the one-way door. MEASURED the same day: tv_spill_dir_free_bytes daily MINIMUM hit 0.0 GB on 2026-08-24, 7.2 GB on 2026-08-31 and 2.4 GB on 2026-09-01, so the Quote 19 grow to 300 bought six days. A full session consumes ~307 GB against the ~309.6 GB a 300 GiB volume presents - the overnight archival reclaims the whole session every night, which is why the volume boots healthy and dies by close, and the margin is ~2 GB. tv-prod-disk-fill-rate-high was FIRING at 135.7 percent per day against a threshold of 4.0. Only SIZE is exhausted: peaks stay 1,168 of 6,000 IOPS (19%) and 107 of 500 MiB/s (21%), so Quote 17 I/O provisioning is NOT reverted to fund this. 500 GiB presents ~524 GB, giving ~217 GB of margin against the measured session - a fix for the MARGIN, not for the BURN, which is ~80% depth. BUDGET read live: limit_amount $150, 90% STOP_EC2_INSTANCES action line $135.00, September forecast $114.01, forecast+grow $132.25, margin $2.75 - so the next addition of any size needs a LEVER, not a cost note. 500 is now BOTH the default and the validation ceiling. INSTANCE change deliberately NOT taken: RSS was 0.29-1.54 GiB across the whole session and spiked to 15.54 GiB in ONE five-minute bucket on a WAL replay of 151 segments / 2,309,027 frames / 22,248,540 depth rows, a bounded burst with a code cause; CPU averaged 12-13% on 4 vCPU. Superseded lock 2026-08-25 (daily-universe-scope-expansion-2026-05-27.md Quote 19 - 'go ahead with your eocmmendation dude see clelary ntoe i never evr want ot face rpessure flushign espielclay entilrey rleated to db questdb evryhtign i shoduld alwyas achieve O(1) dude okay?', given after the 200 GB volume filled MID-SESSION: QuestDB's O3 merge hit CairoException [28] No space left at 11:29 IST and WAL-SUSPENDED 14 tables incl. ticks, market_depth and every candle frame, which keep ACKing ILP writes while silently discarding them; by 11:51 IST ssm send-command failed in 0.001s because the agent could not allocate scratch space, so the box was unmanageable. Only SIZE was exhausted - measured peaks were 1,168 of 6,000 IOPS (19%) and 107 of 500 MiB/s (21%), which is why Quote 17's I/O provisioning is NOT reverted to fund this. The pressure archiver ran correctly, shrank the hot window to its 2-day floor, could not reclaim enough, and raised STORAGE-GAP-05: two days of data no longer fit in 200 GB, which no retention setting can change. Raised from the 2026-08-19 Quote 16 default of 200. 300 is now BOTH the default and the validation ceiling, so any further grow needs a validation edit AND its own dated quote. BUDGET, stated plainly rather than waved through: +100 GB = +$9.12/mo takes a maximal month (22 weekdays x $4.06 measured + 8 weekend days x $2.48) from $109.16 to $118.28. That is UNDER the Quote 18 hard cap of $125 but $1.28 ABOVE the budget's 90% STOP_EC2_INSTANCES action line of $117.00 at the live $130 limit_amount. Quote 19 deliberately does NOT authorize raising limit_amount - Quote 18 forbids it above 125, and 90% of 125 is $112.50 which is below the bill, so the two cannot be reconciled by a ceiling edit alone. The levers are the Quote 10 Elastic IP release (-$3.60/mo) or an operator decision; both are recorded in Quote 19 and neither is taken here. gp3 grows online but can NEVER shrink."
  }
}

# 2026-08-08 (operator Quote 13) — DEFAULT RAISED 20 -> 100 GB.
#
# Sized for the 13-timeframe (1s/5s/10s/15s/30s + 1m/2m/3m/5m/15m/30m/60m + 1d)
# current-day workload WITH raw-tick retention at ~25,000 instruments:
#   ticks     ~25-80 M rows/day (ASSUMED - swings the estimate 3x) => 44-141 GB/mo
#   13 TFs    ~46 M rows/day sparse                               => ~61 GB/mo
# with ~30 days held on disk and the rest archived to S3 by the partition manager.
#
# The sparse figure is VERIFIED, not assumed: live_candle_state.rs:105 makes an
# unopened bucket a sentinel that emits nothing, which is the difference between
# ~46 M rows/day (~2,050/sec, inside the ~5,000/sec ingest envelope) and a dense
# 808 M rows/day (~35,900/sec, 7x over).
#
# WHY 100 AND NOT 250, deliberately: gp3 grows ONLINE in one command but can
# NEVER shrink (§7 Mechanical Rule 3). Starting small is therefore the only
# reversible direction - if the real tick volume lands at the top of the assumed
# range, grow it live; if it lands low, nothing was wasted. Oversizing "to be
# safe" is the one mistake here that cannot be undone without another recreate.

variable "ebs_gp3_iops" {
  description = "Root gp3 EBS provisioned IOPS. RAISED 3000 -> 6000 on 2026-08-19 per operator Quote 17 (daily-universe-scope-expansion-2026-05-27.md §0), alongside throughput 125 -> 500. Range 3000-16000. WHAT TERRAFORM DOES WITH THIS: nothing, to a running box — root_block_device[0].iops sits in the instance's lifecycle.ignore_changes (main.tf), so `terraform apply` never touches the live volume. This variable documents FRESH-PROVISION intent only; the LIVE change is an out-of-band `aws ec2 modify-volume --volume-id <id> --iops 6000 --throughput 500` (or scripts/aws-upgrade-instance.sh), online with no stop, and until that runs the live volume keeps its current settings. Same shape as the Quote 16 size raise. COST: gp3 charges $0.005 per provisioned IOPS above the free 3000 baseline, so (6000-3000) x $0.005 = $15.00/mo."
  type        = number
  default     = 6000

  validation {
    condition     = var.ebs_gp3_iops >= 3000 && var.ebs_gp3_iops <= 16000
    error_message = "ebs_gp3_iops must be 3000-16000 (gp3 range; 3000 is the free baseline)."
  }
}

variable "ebs_gp3_throughput" {
  description = "Root gp3 EBS throughput in MiB/s. RAISED 125 -> 500 on 2026-08-19 per operator Quote 17. Range 125-1000. WHY IT IS THE LOAD-BEARING HALF: dirty_background_ratio = 3 on a 32 GiB host lets ~1 GiB of dirty pages accumulate before writeback starts; draining that at the 125 MiB/s baseline is ~8 seconds of saturated device, during which the ILP flush blocks, the frame drain blocks behind it, the socket receive buffer fills, and Dhan skips a slow consumer forward to the latest available state — dropping intermediate ticks at THEIR side with no sequence number for us to detect it. 500 MiB/s takes the same drain to ~2 seconds. Already binding: 74% NVMe utilisation at 3,121 writes/sec was measured 2026-08-18, before the 25,000-instrument target and before depth persistence. WHAT TERRAFORM DOES WITH THIS: nothing, to a running box — root_block_device[0].throughput is in the instance's lifecycle.ignore_changes (main.tf), so `terraform apply` never touches the live volume. This variable documents FRESH-PROVISION intent only; the LIVE change is an out-of-band `aws ec2 modify-volume --volume-id <id> --iops 6000 --throughput 500`, online with no stop. COST: gp3 charges $0.040 per provisioned MiB/s above the free 125 baseline, so (500-125) x $0.040 = $15.00/mo. Combined with the IOPS raise: $30.00/mo (~₹3,043 incl GST) — HIGHER than the ~$20 the recommendation quoted the operator, because that figure predated the per-unit split; recorded rather than quietly absorbed."
  type        = number
  default     = 500

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
