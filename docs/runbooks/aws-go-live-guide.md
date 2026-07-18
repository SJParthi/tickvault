# AWS Go-Live Guide — copy-paste deploy (2026-05-30)

> **Authority:** This is the operator copy-paste sequence for the FIRST AWS
> deploy. It reflects the current locked Terraform (m8g.large / EBS 30 GB /
> `enable_eip=false` / weekday 08:30–16:30 IST / $25 budget / AL2023 arm64).
> **You run these on your Mac** (this repo's container has no aws/terraform/gh).
> Paste each command's output back to Claude; it troubleshoots each step live.

---

## What gets created (17 resources, ~₹2,058/mo all-in incl GST @ 270 hrs)

VPC + subnet + IGW · Security group · EC2 **m8g.large** (Graviton4, 2 vCPU,
8 GiB) AL2023 arm64 + **EBS gp3 30 GB** encrypted · IAM instance role + GitHub
OIDC role · SNS topic + email sub · EventBridge **weekday** start/stop crons
(08:30 / 16:30 IST) · CloudWatch alarms + log group · AWS Budget **$25** ·
S3 cold bucket (5-yr SEBI lifecycle) · 2 Lambdas (telegram-webhook,
budget-killswitch; claude-triage RETIRED 2026-07-18 — deleted, never
provisioned; see git history). **Elastic IP is NOT created** (`enable_eip=false`).

---

## Step 0 — prerequisites (one-time, on your Mac)

```bash
brew install awscli terraform gh jq        # all four
aws --version          # v2.x
gh auth login          # authorize the SJParthi/tickvault repo
cd ~/tickvault && git pull origin main
```

## Step 1 — AWS account + bootstrap IAM key (human-only, ~40 sec)

1. AWS Console → **IAM → Users → Create user** `tv-terraform-bootstrap`,
   attach **AdministratorAccess** (temporary — deleted in Step 6).
2. That user → **Security credentials → Create access key → CLI** → copy
   both values.
3. On your Mac:
   ```bash
   aws configure          # paste Access Key + Secret; region = ap-south-1 ; output = json
   aws sts get-caller-identity     # must print your account — proves keys work
   ```

## Step 2 — deploy (one command)

```bash
cd ~/tickvault
export TF_VAR_operator_email=sjparthi93@gmail.com
export TF_VAR_operator_cidr=$(curl -s ifconfig.me)/32   # your IP → locks SSH to you
make aws-deploy
```

`make aws-deploy` runs `scripts/aws-one-shot-bootstrap.sh`, which:
pushes `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (+ operator email/CIDR) to
GitHub Secrets → triggers the `terraform-apply.yml` workflow → tails it live →
prints `elastic_ip` (will be empty — EIP disabled), `instance_id`, and
`github_oidc_role_arn`.

> **What the workflow does:** creates the S3 state bucket + DynamoDB lock
> (idempotent), `terraform init/plan/apply`. First run = ~17 resources.
> It has a **market-hours guard** (refuses apply 09:15–15:30 IST on a weekday
> unless you pass `confirm_market_hours=yes`) — deploy outside those hours, or
> on a weekend, to avoid the guard.

## Step 3 — confirm the SNS email

AWS emails `sjparthi93@gmail.com` a **"Subscription Confirmation"** — click the
link. Until you do, CloudWatch alarms + budget alerts can't reach you.

## Step 4 — seed the Dhan secrets into SSM

The app reads all secrets from SSM at boot. Seed them once (interactive,
silent prompts for SecureString):

```bash
cd ~/tickvault
./scripts/aws-seed-ssm-parameters.sh prod      # prompts for each value below
```

Seeds `/tickvault/prod/*`: `dhan/client-id`, `dhan/client-secret`,
`dhan/totp-secret`, `questdb/pg-user`, `questdb/pg-password`,
`telegram/bot-token`, `telegram/chat-id`, `api/bearer-token`,
`network/static-ip` (the EIP — **skip/blank while `enable_eip=false`**),
`sns/phone-number` (optional, for SMS).

## Step 5 — verify the app booted

```bash
make aws-ssm-command       # runs `journalctl -u tickvault -n 50` on the box via SSM
make aws-ssm               # or open an interactive Session Manager shell
# inside the box: make doctor   → 7-section health (QuestDB, auth, feed, …)
```

Healthy = QuestDB up, Dhan auth OK (token via SSM→TOTP), main-feed WebSocket
connected. Note: weekdays the instance auto-starts 08:30 IST and stops 16:30
IST; outside that window start it manually (`aws ec2 start-instances`) for setup.

## Step 6 — security cleanup (after first apply succeeds)

```bash
./scripts/aws-one-shot-bootstrap.sh --upgrade-to-oidc
```

Adds `AWS_TERRAFORM_ROLE_ARN` (the OIDC role) to GitHub Secrets and **deletes
the long-lived bootstrap keys** from GitHub + IAM. All future deploys use
short-lived OIDC tokens. (Manual fallback: copy `github_oidc_role_arn` from
the first run's logs → GitHub secret `AWS_TERRAFORM_ROLE_ARN`; delete the IAM
access key.)

---

## ⚠️ Two things deferred until JUST BEFORE live orders (not now)

1. **Elastic IP is OFF** (`enable_eip=false`) — correct for the 3-month
   no-orders data-pull. Dhan's static-IP order mandate (April 2026) needs it
   ON before you place ANY real order: set `enable_eip=true`, re-apply, then
   `POST /v2/ip/setIP` with the new EIP. **7-day Dhan cooldown** on IP changes —
   do this ≥ 7 days before going live.
2. **`dry_run=true`** stays until you explicitly flip it. No real orders fire
   until then.

---

## ⚠️ Known doc/asset caveats to confirm at deploy time

- **`terraform-apply.yml` pins `TF_VAR_ami_id: ami-0fa0340d4a8bdd6ee`** (a
  fixed AL2023 arm64 AMI for ap-south-1). If `terraform apply` errors with
  `InvalidAMIID.NotFound` (AMIs get deregistered over time), refresh it:
  `aws ssm get-parameters --region ap-south-1 --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 --query 'Parameters[0].Value' --output text`
  then update the `TF_VAR_ami_id` line in the workflow (or pass via dispatch).
- **`deploy/aws/terraform/BOOTSTRAP-ONE-TIME.md` is stale** (still says
  t4g.medium / 10 GB / $12 / 08:00–17:00). The **Terraform itself is correct**
  (m8g.large / 30 GB / $25 / weekday 08:30–16:30) — trust `variables.tf` +
  `main.tf` + `budget.tf`, not that one doc. (Cleanup candidate for a later PR.)

---

## If a step fails — runbook map

| Symptom | Runbook |
|---|---|
| `InsufficientInstanceCapacity` | `docs/runbooks/aws-capacity-error.md` |
| cloud-init / user-data boot failure | `docs/runbooks/aws-cloudinit-failure.md` |
| app OOM during boot | `docs/runbooks/aws-oom-during-boot.md` |
| EventBridge start/stop didn't fire | `docs/runbooks/aws-eventbridge-rule-missing.md` |
| `StartInstances` failed | `docs/runbooks/aws-startinstances-failed.md` |
| app exit-loops under systemd | `docs/runbooks/aws-tickvault-exit-loop.md` |
| full DR (instance/volume loss) | `docs/runbooks/aws-disaster-recovery.md` |
| `AccessDenied` on first workflow run | re-paste keys in GitHub Secrets |
| `BucketAlreadyOwnedByYou` / DynamoDB `AlreadyExists` | safe — idempotent, re-run workflow |

Paste the failing command's output to Claude; it maps to the runbook + gives
the exact next command.
