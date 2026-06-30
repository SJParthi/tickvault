# ONE-TIME bootstrap (40 seconds, forever) — AWS access for GitHub Actions

> **Charter:** every subsequent infra change is 100% automated via `git push`. This is the ONE mathematical event AWS IAM requires from the account owner. After these 40 seconds, you NEVER touch AWS authentication again.

## Recommended path — `aws-one-shot-bootstrap.sh` (single command)

After creating the IAM access key in the AWS Console (step 1 below) and running `aws configure` on your Mac:

```bash
./scripts/aws-one-shot-bootstrap.sh
```

This script does **everything**: reads your AWS keys from `~/.aws/credentials`, pushes them to GitHub Secrets, triggers `terraform-apply.yml`, tails the run live, extracts the Elastic IP + instance ID + OIDC role ARN, and tells you the next steps. After first success, run `./scripts/aws-one-shot-bootstrap.sh --upgrade-to-oidc` to do the security cleanup automatically.

**Prerequisites:** `aws` CLI v2, `gh` CLI (`gh auth login`), `jq`. All standard Mac dev tools.

If you don't have `gh` installed and don't want to install it, the 5-step manual procedure below still works.

## The 3 manual steps (fallback when `gh` CLI is not available)

| # | Where | Click / paste | Time |
|---|---|---|---|
| 1 | **AWS Console → IAM → Users** | Create user `tv-terraform-bootstrap`. Attach policy `AdministratorAccess`. Security credentials tab → Create access key → CLI use case → copy both values. | ~20 sec |
| 2 | **GitHub → Settings → Secrets and variables → Actions** | New repository secret: `AWS_ACCESS_KEY_ID` = (paste). Second secret: `AWS_SECRET_ACCESS_KEY` = (paste). | ~10 sec |
| 3 | **Push any commit touching `deploy/aws/terraform/**`** | e.g. raise `limit_amount` in `budget.tf` from `"12"` to `"13"` and `git push`. GitHub Actions auto-fires `terraform-apply.yml`. First run creates everything. | ~10 sec push |

After step 3 succeeds, **delete the access key from IAM** (Security best practice — long-lived keys are forbidden by the charter beyond bootstrap):

| # | Where | Click | Time |
|---|---|---|---|
| 4 | **AWS Console → IAM → Users → tv-terraform-bootstrap → Security credentials** | Delete the access key. The OIDC role created by Terraform takes over for all future runs. | ~10 sec |
| 5 | **GitHub → Settings → Secrets** | Add new secret `AWS_TERRAFORM_ROLE_ARN` = (the value from `terraform output github_oidc_role_arn` in the first run's logs) | ~10 sec |

**Grand total: ~60 seconds, ONCE in the project's lifetime. Beyond that — zero manual touches forever.**

## What the workflow does on first run

```
git push → GitHub Actions terraform-apply.yml fires
  ├─ Uses AWS_ACCESS_KEY_ID/SECRET (your bootstrap)
  ├─ scripts/aws-bootstrap-state-backend.sh runs
  │   ├─ Creates S3 bucket  tv-terraform-state-<account-id>  (idempotent)
  │   └─ Creates DynamoDB   tv-terraform-locks                (idempotent)
  ├─ terraform init → uses S3 backend with the new bucket
  ├─ terraform plan → 17 resources to create
  ├─ terraform apply → creates EVERYTHING:
  │   ├─ VPC + subnet + IGW
  │   ├─ Security group (SSH from 0.0.0.0/0 — tighten via TF_VAR_operator_cidr later)
  │   ├─ IAM role for EC2 instance
  │   ├─ IAM role for GitHub OIDC (this replaces your access key!)
  │   ├─ EC2 m8g.large AL2023 arm64 + EBS gp3 30GB encrypted
  │   ├─ Elastic IP — count-gated, DEFAULT OFF (enable_eip=false; flip true before live orders)
  │   ├─ SNS topic + email subscription (check inbox, click confirm)
  │   ├─ EventBridge cron 08:30/16:30 IST weekday-only (MON-FRI) start/stop
  │   ├─ 5 CloudWatch alarms (status×2, CPU, EBS, network)
  │   ├─ AWS Budget at $55 USD (~₹2,058/mo all-in incl GST)
  │   ├─ S3 cold bucket with 5-year SEBI lifecycle
  │   └─ CloudWatch log group (14d retention)
  └─ SNS publish → Telegram "Terraform apply OK ... instance_id=i-... elastic_ip=X.X.X.X"
```

## After first apply — the security cleanup

The bootstrap access key has `AdministratorAccess` — too powerful to keep live forever. Replace it with the OIDC role (least-privilege, short-lived tokens):

1. Open the first successful workflow run's logs in GitHub Actions
2. Find the `terraform apply` step
3. Look for `github_oidc_role_arn = "arn:aws:iam::<account>:role/tv-prod-github-deploy-role"`
4. Add it as GitHub secret `AWS_TERRAFORM_ROLE_ARN`
5. Delete the access key in IAM Console

Subsequent workflow runs will see `AWS_TERRAFORM_ROLE_ARN` is set and use OIDC. The bootstrap keys are no longer needed.

## What if something goes wrong on first run

| Symptom | Diagnosis | Fix |
|---|---|---|
| Workflow fails: `AccessDenied` calling sts:GetCallerIdentity | Access key wrong/typo | Re-paste keys in GitHub Secrets |
| Workflow fails: `BucketAlreadyOwnedByYou` | S3 bucket exists from a prior partial bootstrap | Safe — script is idempotent, re-run the workflow |
| Workflow fails: `AlreadyExistsException` on DynamoDB | Same | Safe — script is idempotent, re-run the workflow |
| Workflow fails: terraform plan returns error about EIP cooldown | You previously created + deleted an EIP within 7 days | Wait 7 days, OR import the existing EIP into state |
| `terraform apply` succeeds but no Telegram message | SNS confirmation email not clicked yet | Check inbox for confirmation email from AWS, click confirm |

## The "everywhere" promise — restated

After bootstrap:

| Surface | How it talks to AWS |
|---|---|
| `git push` to main with infra changes | terraform-apply.yml runs via OIDC, applies, Telegram notifies |
| IntelliJ AWS Toolkit | Uses AWS SSO or local `~/.aws/credentials` (PR #774 wires this) |
| Claude Code session | tickvault-aws MCP server (PR #773) — short-lived OIDC tokens |
| Claude Cowork task | Same MCP server, auto-loaded via .mcp.json |
| Claude Chat (mobile) | Same MCP server, inherits MCP from claude.ai cloud |
| Deploy a new Rust binary | deploy-aws.yml (already exists) — uses OIDC, blue/green via PR #772 |

All 6 surfaces share the SAME OIDC trust. Your one 40-second bootstrap unlocks all of them.

## Honest envelope

> 100% inside the tested envelope: after the 40-second bootstrap (steps 1+2), every infra change is fully automated via `git push`. Steps 4+5 (delete keys, switch to OIDC) are optional security hardening — the workflow works either way, but charter §C "100% security hardening" requires the migration to OIDC within the first deploy week. Beyond the envelope: if your AWS account gets locked, if the GitHub Actions service goes down, or if Dhan's static-IP cooldown traps you — those are external systems with their own SLAs. No silent failure modes: every failure surfaces as a Telegram message via the SNS topic.
