# DLT AWS Terraform — Phase 8.1

Single-instance deployment of `tickvault` to AWS ap-south-1 (Mumbai)
per `.claude/rules/project/aws-budget.md` — total monthly cost **₹4,981**
under the ₹5,000 cap.

## What this creates

- **1 VPC** (`10.42.0.0/16`) with 1 public subnet in ap-south-1a
- **1 EC2 instance** — `c7i.xlarge` Ubuntu 24.04 LTS, gp3 100GB root
- **1 Elastic IP** — static public IP (required for Dhan static-IP mandate, effective 2026-04-01)
- **1 Security Group** — SSH from `operator_cidr`, no other inbound
- **1 IAM role** — SSM get/put, SNS publish, CloudWatch write, S3 read/write to cold bucket
- **4 EventBridge rules** — weekday/weekend start/stop per budget schedule
- **1 SNS topic** — CRITICAL alerts fan-out (operator subscribes Telegram webhook)
- **1 CloudWatch log group** — 14-day retention for app + system logs
- **1 S3 bucket** — cold archive, 30-day lifecycle to Intelligent-Tiering,
  365-day to Glacier IR, 5-year expiration (SEBI retention)

## One-time setup BEFORE first `terraform apply`

1. **Create AWS account.** Enable MFA on root. Attach an Indian credit card. Set a billing alert at ₹4,500 (90% of budget).
2. **Request limit increase** for `c7i.xlarge` in ap-south-1 (default quota is 0 for new accounts).
3. **Create a key pair** for SSH:
   ```bash
   aws ec2 create-key-pair \
     --key-name tv-prod-key \
     --region ap-south-1 \
     --query KeyMaterial --output text > ~/.ssh/tv-prod-key.pem
   chmod 400 ~/.ssh/tv-prod-key.pem
   ```
4. **Find the latest Ubuntu 24.04 LTS AMI** in ap-south-1:
   ```bash
   aws ec2 describe-images \
     --region ap-south-1 \
     --owners 099720109477 \
     --filters 'Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*' \
     --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text
   ```
   Set the result as `TF_VAR_ami_id`.
5. **Find your public IP** and set `TF_VAR_operator_cidr`:
   ```bash
   export TF_VAR_operator_cidr="$(curl -s ifconfig.me)/32"
   ```
6. **Install Terraform** >= 1.9.0.

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
2. **Seed SSM parameters** with secrets.
3. **Trigger the GitHub Actions `deploy-aws` workflow** to scp the first
   binary and `systemctl start tickvault`.

## Cost

Exact monthly breakdown (ON-DEMAND, ap-south-1):

| Component | Spec | Rupees/mo |
|---|---|---|
| EC2 (weekdays 9hr x 22 days) | c7i.xlarge @ $0.1785/hr | 3,530 |
| EC2 (weekends 5hr x 8 days) | c7i.xlarge @ $0.1785/hr | 607 |
| EBS gp3 100GB | @ $0.0912/GB-mo | 775 |
| S3 Intelligent-Tiering (up to 500GB) | Worst case | 333 |
| Elastic IP (always attached) | $0.005/hr | 152 |
| CloudWatch (7 metrics + 5 alarms + 2GB logs) | Free tier | 0 |
| SNS (100 SMS/mo India) | $0.00278/msg | 25 |
| Data Transfer (~10GB egress) | ~$0.01/GB | 85 |
| **TOTAL** | | **4,981** |

Buffer under the Rs.5,000 cap: **Rs.19**.

## Destroy

```bash
terraform destroy
```

**Warning:** this deletes the EIP. The new EIP will be different and needs
re-registration with Dhan. Only destroy if you're sure — prefer `aws ec2
stop-instances` for temporary pauses.

## Honest limitations

- **Single instance, no HA.** A c7i.xlarge fault = ~30 seconds of unplanned downtime while EC2 replaces. Budget does not permit 2-instance HA.
- **No blue-green deploy.** Deploys happen outside 09:15-15:30 IST via the scheduled stop window.
- **No automated state backup to S3 for Terraform state.** The `backend "s3"` block is commented out — after first apply, operator uncomments and runs `terraform init -migrate-state`.
- **AMI ID placeholder.** `var.ami_id` defaults to `ami-placeholder-replace-me`. You MUST override before `terraform apply`.
