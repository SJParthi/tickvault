# AWS Deploy Runbook — First-Time Setup

> **Target date:** May 1, 2026 — AWS go-live.
> **Owner:** Parthiban (you).
> **Scope:** Everything from "no AWS account" to "tickvault running in ap-south-1".

This runbook covers the **one-time** steps that only a human can do
(creating accounts, attaching credit cards, clicking "I accept"). After
this runbook is complete, ongoing deploys are fully automated via the
`deploy-aws` GitHub Actions workflow — no more manual steps.

## Prerequisites (local machine)

- Terraform >= 1.9.0 (`brew install terraform` or `apt install terraform`)
- AWS CLI v2 (`brew install awscli`)
- Access to this repo (`git clone`)
- A valid Indian credit/debit card for AWS billing
- Your current public IP (`curl ifconfig.me`)

## Phase 1 — AWS Account Creation (~20 minutes, human only)

1. **Create AWS account**
   - Go to https://aws.amazon.com/ and click **Create an AWS Account**
   - Email: use a dedicated one (e.g., `tv-ops@yourdomain.com`)
   - Password: 16+ characters, stored in a password manager
   - Account name: `tv-prod`
   - Payment: attach the Indian credit card. AWS pre-authorises Rs 2
     as verification — this gets refunded.
2. **Enable MFA on the root user immediately**
   - Sign in as root, go to IAM → My security credentials → MFA
   - Use a TOTP app (Google Authenticator, Authy, 1Password)
   - **After this step, lock the root credentials away. You will
     almost never use root again.**
3. **Create an IAM user for ongoing operations**
   - IAM → Users → Create user → `tv-admin`
   - Enable programmatic access + console access
   - Attach policy: `AdministratorAccess` (we'll narrow later)
   - Enable MFA on this user too
   - Save the access key ID + secret key in a password manager
4. **Configure AWS CLI locally**
   ```bash
   aws configure
   # AWS Access Key ID: <from step 3>
   # AWS Secret Access Key: <from step 3>
   # Default region name: ap-south-1
   # Default output format: json
   ```
5. **Set a billing alert at Rs 4,500 (90% of the Rs 5,000 budget)**
   - Billing Dashboard → Budgets → Create budget
   - Budget type: Cost budget
   - Amount: Rs 4500, Monthly, Recurring
   - Email alert on 90% forecasted + 100% actual
6. **Request limit increase for c7i.xlarge in ap-south-1**
   - New AWS accounts default to 0 running On-Demand Standard
     instances (the family that includes c7i).
   - Service Quotas → AWS services → EC2 → Running On-Demand Standard
     (A, C, D, H, I, M, R, T, Z) instances
   - Click **Request increase at account level**
   - New quota value: `8` (enough for c7i.xlarge = 4 vCPU, with
     headroom for a blue-green future upgrade)
   - Justification: "Running a production trading system in
     ap-south-1 on c7i.xlarge per the fixed deployment spec"
   - **This takes 1-24 hours for AWS to approve.** Start now.

## Phase 2 — Bootstrap Secrets (~10 minutes)

1. **Create the EC2 key pair**
   ```bash
   aws ec2 create-key-pair \
     --key-name tv-prod-key \
     --region ap-south-1 \
     --query KeyMaterial --output text > ~/.ssh/tv-prod-key.pem
   chmod 400 ~/.ssh/tv-prod-key.pem
   ```
2. **Look up the latest Ubuntu 24.04 AMI in ap-south-1**
   ```bash
   AMI_ID=$(aws ec2 describe-images \
     --region ap-south-1 \
     --owners 099720109477 \
     --filters 'Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*' \
     --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
   echo "Latest Ubuntu 24.04 AMI: $AMI_ID"
   ```
3. **Store Dhan + Telegram secrets in SSM Parameter Store**
   ```bash
   aws ssm put-parameter \
     --region ap-south-1 \
     --name /tickvault/prod/dhan/client-id \
     --type SecureString \
     --value 'YOUR_DHAN_CLIENT_ID'

   aws ssm put-parameter \
     --region ap-south-1 \
     --name /tickvault/prod/dhan/totp-secret \
     --type SecureString \
     --value 'YOUR_TOTP_BASE32_SECRET'

   aws ssm put-parameter \
     --region ap-south-1 \
     --name /tickvault/prod/telegram/bot-token \
     --type SecureString \
     --value 'YOUR_TELEGRAM_BOT_TOKEN'

   aws ssm put-parameter \
     --region ap-south-1 \
     --name /tickvault/prod/telegram/chat-id \
     --type SecureString \
     --value 'YOUR_CHAT_ID'
   ```

## Phase 3 — Terraform Apply (~15 minutes)

1. **Export required variables**
   ```bash
   export TF_VAR_ami_id="$AMI_ID"
   export TF_VAR_operator_cidr="$(curl -s ifconfig.me)/32"
   ```
2. **Initialise Terraform**
   ```bash
   cd deploy/aws/terraform
   terraform init
   ```
3. **Review the plan** — should show ~15 resources to create.
   ```bash
   terraform plan
   ```
4. **Apply**
   ```bash
   terraform apply
   # Type: yes
   ```
5. **Capture the outputs**
   ```bash
   terraform output
   # Note: instance_id, elastic_ip, ssm_session_command
   ```

## Phase 4 — Dhan Static IP Registration (~5 minutes)

The Elastic IP from Phase 3 must be registered with Dhan before you can
place orders (the static IP mandate took effect 2026-04-01).

```bash
# Get the EIP
EIP=$(terraform -chdir=deploy/aws/terraform output -raw elastic_ip)
echo "Registering $EIP with Dhan..."

# From your local machine with a valid access-token:
curl -X POST https://api.dhan.co/v2/ip/setIP \
  -H "access-token: $DHAN_ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"dhanClientId\":\"$DHAN_CLIENT_ID\",\"ip\":\"$EIP\",\"ipFlag\":\"PRIMARY\"}"
```

**CRITICAL:** Dhan's IP modification has a **7-day cooldown**. Get this
right the first time. Verify the registration:

```bash
curl https://api.dhan.co/v2/ip/getIP \
  -H "access-token: $DHAN_ACCESS_TOKEN"
# Expected: ordersAllowed: true, ipMatchStatus: MATCH
```

## Phase 5 — First Deploy (~10 minutes, automated)

1. **Configure GitHub Actions OIDC role**
   - Create an IAM role `tv-github-deploy-role` that the
     GitHub Actions workflow can assume. See `docs/runbooks/github-oidc-setup.md`
     (TODO — for now, use long-lived credentials as a temporary workaround).
2. **Store GitHub secrets**
   - Repo Settings → Secrets and variables → Actions
   - Secrets: `AWS_ROLE_ARN`, `AWS_ACCOUNT_ID`, `EC2_INSTANCE_ID`
   - Variables: `AWS_REGION = ap-south-1`
3. **Trigger the workflow manually**
   - GitHub → Actions → deploy-aws → Run workflow
   - Select branch: main
   - Wait ~5 minutes
4. **Verify the app is running**
   ```bash
   aws ssm start-session \
     --region ap-south-1 \
     --target $(terraform -chdir=deploy/aws/terraform output -raw instance_id)
   # In the session:
   systemctl status tickvault
   journalctl -u tickvault -f
   ```

## Phase 6 — Smoke Test (~5 minutes)

1. **Check Grafana is reachable**
   - SSH port-forward: `ssh -L 3000:localhost:3000 -i ~/.ssh/tv-prod-key.pem ubuntu@$EIP`
   - Browse to http://localhost:3000 (admin/admin first login)
   - Verify the 4 dashboards load and show data
2. **Verify Prometheus scrapes tv-app**
   - Grafana → Explore → Prometheus datasource → `tv_questdb_connected`
   - Should return 1.0
3. **Verify Telegram alerts**
   - Trigger a test alert: stop QuestDB temporarily
     ```bash
     docker stop tv-questdb
     sleep 40  # wait for S3-1 30s degraded threshold
     ```
   - Confirm Telegram receives the CRITICAL alert
   - Restart QuestDB: `docker start tv-questdb`

## Phase 7 — Schedule Validation (~5 minutes)

The EventBridge rules should auto-start/stop the instance per the budget
schedule. Verify they're installed:

```bash
aws events list-rules --region ap-south-1 --name-prefix tv-prod
# Expected: 4 rules (weekday-start, weekday-stop, weekend-start, weekend-stop)
```

Test the stop by manually firing a rule and confirming the instance stops:

```bash
aws events test-event-pattern \
  --region ap-south-1 \
  --event-pattern '{"source":["aws.events"]}' \
  --event '{"source":"aws.events","account":"'$AWS_ACCOUNT_ID'","time":"2026-05-01T11:30:00Z"}'
```

## What's next (no longer manual)

Once this runbook is complete, all future deploys use the
`deploy-aws` GitHub Actions workflow. Your workflow becomes:

1. `git push` or tag a release
2. Actions runs build + smoke test + deploy + monitor
3. Telegram fires on success or failure
4. If any step fails, automatic rollback

No more manual steps until the next Dhan credential rotation (once per
year) or an AWS-side emergency.

## Rollback plan for a broken first deploy

If Phase 5 fails:

```bash
# Option A: Stop the instance
aws ec2 stop-instances --region ap-south-1 --instance-ids $INSTANCE_ID

# Option B: Full destroy + re-apply
cd deploy/aws/terraform
terraform destroy
# Fix whatever broke, then terraform apply again
```

See `aws-disaster-recovery.md` for post-go-live DR procedures.
