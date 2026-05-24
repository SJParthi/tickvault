# tickvault AWS Terraform

Single-instance deployment of `tickvault` to AWS ap-south-1 (Mumbai)
per `.claude/rules/project/aws-budget.md` — **~₹1,022/mo** on `t4g.medium`
(operator-lock 2026-05-18).

## What this creates

- **1 VPC** (`10.42.0.0/16`) with 1 public subnet in ap-south-1a
- **1 EC2 instance** — `t4g.medium` (ARM Graviton, 2 vCPU / 4 GiB) Amazon Linux 2023 arm64, gp3 10GB root
- **1 Elastic IP** — static public IP (required for Dhan static-IP mandate, effective 2026-04-01; 7-day cooldown on modify — never release once registered)
- **1 Security Group** — SSH from `operator_cidr`, no other inbound
- **1 IAM role** — SSM get/put/delete (instance lock), SNS publish, CloudWatch write, S3 read/write to cold bucket
- **2 EventBridge rules** — daily 08:00 IST start / 17:00 IST stop (Mon-Sun)
- **1 SNS topic** — CRITICAL alerts → 4-channel fan-out (SMS + Telegram + Email + Connect call)
- **1 CloudWatch log group** — 14-day retention for app + system logs (scrapes `/var/log/messages`, `/opt/tickvault/logs/errors.jsonl*`, `/opt/tickvault/logs/app.*.log`)
- **5 CloudWatch alarms** — infrastructure signals (status check, CPU, disk, memory, network)
- **1 AWS Budget** — monthly cost cap at $12 USD (~₹1,000), 80% forecasted + 100% actual notifications
- **1 SNS email subscription** — operator email auto-subscribed to `tv-alerts` topic (confirmation via email)
- **1 S3 bucket** — cold archive, 30-day lifecycle to Intelligent-Tiering, 365-day to Glacier IR, 5-year expiration (SEBI retention)
- **Termination + stop protection** enabled on EC2 instance (prevents accidental destroy)

**NOT deployed** (CloudWatch-only migration #O1/#O2/#O3/#O4): Grafana, Prometheus, Alertmanager, Valkey. Operator-facing observability = QuestDB Console (local dev) + CloudWatch Console (prod).

## One-time setup BEFORE first `terraform apply`

1. **Create AWS account.** Enable MFA on root. Attach an Indian credit card. (Billing alarm at ₹1,000 / 98% of budget is now provisioned by `budget.tf` — no manual setup needed.)
2. **Request limit increase** for `t4g.medium` in ap-south-1 if your default vCPU quota is 0 (new accounts).
3. **Create a key pair** for SSH:
   ```bash
   aws ec2 create-key-pair \
     --key-name tv-prod-key \
     --region ap-south-1 \
     --query KeyMaterial --output text > ~/.ssh/tv-prod-key.pem
   chmod 400 ~/.ssh/tv-prod-key.pem
   ```
4. **AMI default is set** — `var.ami_id` defaults to `ami-0fa0340d4a8bdd6ee` (AL2023 arm64 ap-south-1, published 2026-05-15). To refresh quarterly:
   ```bash
   aws ec2 describe-images \
     --region ap-south-1 \
     --owners amazon \
     --filters 'Name=name,Values=al2023-ami-2023.*-arm64' \
               'Name=virtualization-type,Values=hvm' \
     --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text
   ```
   Update the default in `variables.tf` if newer. `lifecycle.ignore_changes = [ami]` on the instance prevents existing instances from being rebuilt.

   **Why Amazon Linux 2023 (not Ubuntu)?** AL2023 ships with AWS CLI + amazon-ssm-agent + amazon-cloudwatch-agent **pre-installed**, so the user-data script skips ~30 lines of apt-get installs. ~30s faster cold boot at the daily 08:00 IST EventBridge start. ~150 MB more RAM headroom on the 4 GiB host. The Rust binary, Docker, and tickvault behaviour are byte-identical on either OS.
5. **Find your public IP** and set `TF_VAR_operator_cidr`:
   ```bash
   export TF_VAR_operator_cidr="$(curl -s ifconfig.me)/32"
   ```
6. **Set operator email for alarm notifications** (required):
   ```bash
   export TF_VAR_operator_email="you@example.com"
   ```
   On first apply, AWS sends a confirmation email — click the link to activate SNS alarm delivery.
7. **Install Terraform** >= 1.9.0.
8. **Seed SSM parameters** with Dhan / Telegram / QuestDB credentials BEFORE first boot — run `../../../scripts/aws-seed-ssm-parameters.sh`.
9. **(Optional but recommended)** Migrate Terraform state to S3 backend after first apply — see `BACKEND-BOOTSTRAP.md`.

## First apply

```bash
cd deploy/aws/terraform
terraform init
terraform plan  # review — should show ~15 resources to create
terraform apply # confirm when ready
```

Outputs will show:
- `instance_id` — the EC2 ID for SSM sessions
- `elastic_ip` — **REGISTER THIS WITH DHAN** via
  `POST https://api.dhan.co/v2/ip/setIP` (once; 7-day cooldown on modify)
- `ssh_command` — `ssh -i ...`
- `ssm_session_command` — no-SSH session access

## After first apply

1. **Register the EIP with Dhan** via `POST /v2/ip/setIP` — see
   `.claude/rules/dhan/authentication.md` rule 7. The IP is static and
   modifiable only once per 7 days.
2. **Trigger the GitHub Actions `deploy-aws` workflow** to scp the first
   binary and `systemctl start tickvault`.

## Cost — t4g.medium, daily 08:00–17:00 IST Mon-Sun

| Component | Spec | Rupees/mo |
|---|---|---|
| EC2 t4g.medium (9hr × 30 days) | $0.0224/hr × 270 hr | 514 |
| Elastic IP (always attached, 24/7) | $0.005/hr × 720 hr | 306 |
| EBS gp3 10GB | @ $0.0912/GB-mo | 78 |
| S3 cold (4-SID tiny dataset) | Intelligent-Tier → Glacier | 15 |
| CloudWatch (10 metrics + 10 alarms + 5GB logs) | Free tier | 0 |
| SNS (100 India SMS/mo) | $0.00278/msg | 24 |
| Data Transfer (~10GB egress) | ~$0.01/GB | 85 |
| **TOTAL** | | **~1,022** |

~₹22 over the <₹1,000 aspirational floor — operator-locked Option A (accept overage for 7-day weekend availability for BRUTEX work).

## Destroy

```bash
terraform destroy
```

**Warning:** this deletes the EIP. The new EIP will be different and needs
re-registration with Dhan. Only destroy if you're sure — prefer `aws ec2
stop-instances` for temporary pauses.

## Honest limitations

- **Single instance, no HA.** A t4g.medium fault = ~30 seconds of unplanned downtime while EC2 replaces. Budget does not permit 2-instance HA.
- **No blue-green deploy.** Deploys happen outside 09:15-15:30 IST via the scheduled stop window.
- **No automated state backup to S3 for Terraform state.** The `backend "s3"` block is commented out — after first apply, operator uncomments and runs `terraform init -migrate-state`.
- **AMI ID placeholder.** `var.ami_id` defaults to `ami-placeholder-replace-me-arm64`. You MUST override before `terraform apply` (step 4 above).
- **Burstable CPU.** t4g.medium has 2 vCPU baseline; if the 4-SID workload ever exceeds it (it shouldn't), credits accumulate during off-hours and absorb the spike.
