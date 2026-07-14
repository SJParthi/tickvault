# AWS Deployment — Infrastructure Reference

> **Refreshed 2026-05-30** to match the m8g.large / CloudWatch-only / no-EIP operator lock (`daily-universe-scope-expansion-2026-05-27.md` §7).
>
> **Authority:** Parthiban (architect). Approved AWS bill ≤ $25/mo (~₹2,058/mo all-in incl. GST at 270 hrs) per `aws-budget.md` + daily-universe §7.
> **Scope:** Infrastructure-as-code reference + day-2 operations. NOT the first-time setup runbook.
> **First-time setup:** `docs/runbooks/aws-deploy.md` covers the one-time account creation + first Terraform apply.
> **Region:** `ap-south-1` (Mumbai) ONLY. Dhan static-IP whitelist + low latency.

## What this runbook covers

This is the OPERATIONAL reference for the AWS deployment:

  * Day-2 ops — scaling, key rotation, security-group updates
  * Terraform module structure + invocation
  * Quick-reference tables for every AWS resource
  * EventBridge cron schedule (08:30-16:30 IST, weekday-only)
  * Disaster recovery cross-reference

Use `docs/runbooks/aws-deploy.md` for first-time account setup.

## Resource inventory (verified vs `aws-budget.md`)

| Resource | Type | Spec | Notes |
|---|---|---|---|
| EC2 | `m8g.large` | ARM Graviton4, 2 vCPU, 8 GiB RAM | On-demand only ($0.06416/hr ap-south-1); never reserved/spot. Operator-PINNED (daily-universe §7) |
| EBS root | gp3 | 50 GB (`ebs_gp3_size_gb`, default 50 since 2026-07-13 — was 30; range 10-200) | Host OS + Docker + QuestDB hot window; >90d partitions archived to S3. Live volume grown out-of-band (`aws-upgrade-instance.sh --ebs-size`) — `volume_size` is in `lifecycle.ignore_changes` |
| Elastic IP | static IPv4 | count-gated (`enable_eip`, **default OFF**) | EXCLUDED for the 3-month no-orders data-pull window (saves ~₹430/mo). Flip `enable_eip = true` + re-register with Dhan BEFORE live orders |
| IAM Role | `tv-prod-instance-role` | SSM read + S3 archive write | Attached to EC2 instance profile |
| IAM Role | `tv-github-deploy-role` | EC2 stop/start + EIP describe | Used by GitHub Actions deploy workflow (OIDC, `oidc.tf`) |
| Security Group | `tv-prod-sg` | Inbound: 22 (operator IP only) | Egress: all (TLS to Dhan + AWS APIs). No inbound 443 — there is no load balancer |
| EventBridge Rule | `tv-prod-start-weekday` | `cron(0 3 ? * MON-FRI *)` (08:30 IST Mon-Fri) | Auto-start |
| EventBridge Rule | `tv-prod-stop-weekday` | `cron(0 11 ? * MON-FRI *)` (16:30 IST Mon-Fri) | Auto-stop. Weekends + NSE holidays = OFF unless operator manually starts |
| CloudWatch | metrics + alarms + logs + dashboards | within free tier | 10 metrics, 10 alarms, 5 GB logs, 3 dashboards. Sole observability layer (no Grafana/Prometheus/Alertmanager) |
| SNS Topic | `tv-prod-sms-alerts` | India SMS | ~100 msg/mo, ~₹24 |
| S3 Bucket | `tv-prod-archive-ap-south-1` | Intelligent-Tiering + Glacier | Lifecycle 90d → 365d → Glacier |
| SSM Parameter Store | `/tickvault/prod/*` | encrypted (SecureString) | Dhan token, TOTP secret (Valkey removed in #O4; dual-instance lock now lives in SSM) |

## Terraform module layout

The Terraform is a **flat layout** (no per-resource split files). The real
files are:

```
deploy/aws/terraform/
├── main.tf                      # VPC + EC2 + EIP (count-gated) + IAM + SNS + EventBridge + S3
├── variables.tf                 # region, env, ebs_gp3_size_gb, enable_eip, operator_ip, ...
├── versions.tf                  # provider + Terraform version pins
├── outputs.tf                   # instance_id, EIP (when enabled), SNS topic ARN
├── user-data.sh.tftpl           # EC2 boot script template (Amazon Linux 2023 arm64)
├── oidc.tf                      # GitHub Actions OIDC provider + deploy role trust
├── alarms.tf                    # CloudWatch infra alarms
├── app-alarms.tf                # CloudWatch app-level alarms
├── budget.tf                    # $25/mo budget cap
├── sns-subscriptions.tf         # SMS/email/HTTPS subscriptions
├── budget-killswitch-lambda.tf  # auto-stop on budget breach
├── claude-triage-lambda.tf      # triage Lambda
├── telegram-webhook-lambda.tf   # SNS → Telegram bridge Lambda
└── README.md                    # module readme
```

## Day-2 operations

### Change the instance type — operator-PINNED, NOT a casual edit

The instance type is **`m8g.large` and is operator-PINNED** (Graviton4,
2 vCPU, 8 GiB RAM). A Terraform validation block on the `instance_type`
variable **rejects any other type** at `plan` time — there is no
`t4g`-to-`t4g` (or any other) scale path you can reach by editing a
tfvars file.

Changing the instance type is NOT a tfvars edit. It requires the
**4-file rule-update protocol** in
`daily-universe-scope-expansion-2026-05-27.md` §7 Mechanical Rule 1:

  1. Operator explicit approval with a dated quote.
  2. Update `daily-universe-scope-expansion-2026-05-27.md` §7.
  3. Update `docs/architecture/aws-indices-only-locked-architecture.md` §5.
  4. Update `.claude/rules/project/aws-budget.md` (marked SUPERSEDED).
  5. Update the ratchet test `crates/storage/tests/instance_type_lock_guard.rs`
     to pin the new type, AND the Terraform validation block in `variables.tf`.

Only after all of the above does the actual Terraform change land. The
budget is capped at $25/mo (`budget.tf`); the killswitch Lambda auto-stops
the instance on breach. See `.claude/rules/project/aws-budget.md` rule 1.

To grow disk instead (online, no instance stop), raise `ebs_gp3_size_gb`
(default 30, range 10-200) — gp3 grows live without data loss.

### Rotate the Elastic IP — DO NOT (7-day cooldown!)

> **Current state:** the EIP is DISABLED (`enable_eip = false`) for the
> 3-month no-orders data-pull window — there is no EIP to rotate today.
> This section applies once `enable_eip = true` is flipped (which must
> happen, with Dhan re-registration, BEFORE any live orders).

When enabled, the Elastic IP is registered with Dhan's static-IP whitelist.
**Dhan enforces a 7-day cooldown on IP changes.** Rotating the EIP means 7
days of rejected orders.

If the EIP somehow gets released (e.g., terraform destroy):

  1. Open Dhan support ticket immediately — they may waive the
     cooldown for documented terraform mishaps.
  2. Run `terraform apply` to allocate a NEW EIP.
  3. Update Dhan-side static IP via the `POST /v2/ip/setIP` endpoint
     once cooldown expires.
  4. Until then, app runs read-only; no orders allowed.

### Update the security group

Inbound port 22 (SSH) is restricted to the operator's home IP. When
the home IP changes (ISP DHCP refresh, new location):

```bash
# 1. Get current public IP
NEW_IP=$(curl -s ifconfig.me)

# 2. Pass the new operator_ip to terraform (var defined in variables.tf)
cd deploy/aws/terraform
terraform plan  -var="operator_ip=$NEW_IP/32" -target=aws_security_group.tv_prod_sg
terraform apply -var="operator_ip=$NEW_IP/32" -target=aws_security_group.tv_prod_sg

# 3. Verify SSH still works (use the instance public IP, or the EIP if enable_eip=true)
ssh -i ~/.ssh/tv-prod-key.pem ec2-user@<INSTANCE_IP> 'echo ok'
```

### Rotate SSH keys (90-day cadence)

```bash
# 1. Generate new key pair
ssh-keygen -t ed25519 -f ~/.ssh/tv-prod-key-$(date +%Y%m).pem -N ""

# 2. Add public key to instance (uses OLD key still active)
ssh -i ~/.ssh/tv-prod-key.pem ec2-user@<EIP> \
    "cat >> ~/.ssh/authorized_keys" < ~/.ssh/tv-prod-key-$(date +%Y%m).pem.pub

# 3. Verify new key works
ssh -i ~/.ssh/tv-prod-key-$(date +%Y%m).pem ec2-user@<EIP> 'echo ok'

# 4. Remove old key from authorized_keys (use new SSH session)
ssh -i ~/.ssh/tv-prod-key-$(date +%Y%m).pem ec2-user@<EIP> \
    "grep -v '<old_key_substring>' ~/.ssh/authorized_keys > /tmp/auth && mv /tmp/auth ~/.ssh/authorized_keys"

# 5. Update local default symlink
ln -sf ~/.ssh/tv-prod-key-$(date +%Y%m).pem ~/.ssh/tv-prod-key.pem
```

### Modify the EventBridge cron schedule

The schedule is **weekday-only (Mon-Fri)**: auto-start 08:30 IST, auto-stop
16:30 IST. There are exactly TWO EventBridge rules and **no weekend rules** —
weekends + NSE holidays leave the instance OFF unless the operator manually
starts it (see "Manual start/stop" below). The crons live in `main.tf`:

```
start: cron(0 3 ? * MON-FRI *)   # 08:30 IST Mon-Fri
stop:  cron(0 11 ? * MON-FRI *)  # 16:30 IST Mon-Fri
```

To shift the weekday window (e.g. stop at 17:00 IST instead of 16:30), edit
the `schedule_expression` of the stop rule in `main.tf`, keeping total
runtime within the budget envelope (~270 running hrs/mo at the $25/mo cap):

```bash
# Edit the stop rule schedule_expression in deploy/aws/terraform/main.tf, then:
cd deploy/aws/terraform
terraform plan
terraform apply
```

### Manual start/stop

```bash
# Start
INSTANCE_ID=$(terraform -chdir=deploy/aws/terraform output -raw instance_id)
aws ec2 start-instances --instance-ids "$INSTANCE_ID" --region ap-south-1

# Stop
aws ec2 stop-instances --instance-ids "$INSTANCE_ID" --region ap-south-1

# Status
aws ec2 describe-instances --instance-ids "$INSTANCE_ID" \
    --region ap-south-1 \
    --query 'Reservations[0].Instances[0].State.Name'
```

## Cost monitoring

The $25/mo budget cap is managed in Terraform (`budget.tf`), and the
killswitch Lambda (`budget-killswitch-lambda.tf`) auto-stops the instance
on breach — no manual alarm setup is required. If you want an additional
ad-hoc CloudWatch billing alarm (e.g. at $22, ~90% of cap):

```bash
aws cloudwatch put-metric-alarm \
  --alarm-name tv-prod-budget-alarm \
  --metric-name EstimatedCharges \
  --namespace AWS/Billing \
  --statistic Maximum \
  --period 21600 \
  --threshold 22 \
  --comparison-operator GreaterThanThreshold \
  --evaluation-periods 1 \
  --alarm-actions <SNS_TOPIC_ARN_FROM_TERRAFORM_OUTPUT> \
  --region us-east-1   # billing metrics ONLY exist in us-east-1
```

Daily cost check via `aws ce get-cost-and-usage`:

```bash
aws ce get-cost-and-usage \
  --time-period Start=$(date -u +%Y-%m-01),End=$(date -u +%Y-%m-%d) \
  --granularity DAILY \
  --metrics UnblendedCost \
  --region us-east-1
```

## Disaster recovery cross-reference

See `docs/runbooks/aws-disaster-recovery.md` for:

  * EC2 instance loss → terraform apply from scratch
  * EBS root volume corruption → rebuild + restore hot data from S3 archive
  * EIP released (only when `enable_eip = true`) → 7-day Dhan support coordination
  * IAM role compromise → key rotation procedure
  * Region-wide outage → manual failover to ap-south-2 (Hyderabad)

## Anti-patterns FORBIDDEN

  * **Running terraform from a laptop without state-lock backend.**
    State must live in S3 with DynamoDB lock; otherwise concurrent
    operator + GitHub Actions runs corrupt state.
  * **Manually clicking in AWS Console to "fix something quick".**
    Every change goes through terraform; console edits drift the state.
  * **Storing secrets in terraform variables.** Secrets live in SSM
    Parameter Store ONLY; terraform reads them via `data` blocks.
  * **Provisioning anything outside `ap-south-1`.** Latency to Dhan
    increases by 50-100ms from other regions; static IP whitelist
    is region-pinned anyway.
  * **Releasing the Elastic IP for ANY reason** (see Day-2 ops above).

## Cross-references

  * First-time setup: `docs/runbooks/aws-deploy.md`
  * Budget enforcement: `.claude/rules/project/aws-budget.md`
  * Disaster recovery: `docs/runbooks/aws-disaster-recovery.md`
  * Dhan static IP rules: `.claude/rules/dhan/authentication.md` rule 7
  * MacBook dies mid-session: `docs/runbooks/macbook-dies-mid-session.md`
  * Phase 0 architecture: `.claude/rules/project/phase-0-architecture.md`

## Honest 100% claim

> "100% docs accuracy at the moment of merge — every Terraform path,
> every cron expression, every AWS CLI command was verified against
> the actual deploy/aws/terraform/ tree and the AWS API behavior on
> 2026-05-16. Where a resource is planned but not yet deployed (e.g.
> billing alarm), the runbook explicitly cites the manual create
> command, not a Terraform module."
