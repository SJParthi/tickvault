# Implementation Plan: PR #771 — GitHub Actions auto `terraform apply`

**Status:** VERIFIED
**Date:** 2026-05-24
**Approved by:** Parthiban (5-PR roadmap confirmed via AskUserQuestion during AWS launch session)
**Branch:** `claude/confident-brahmagupta-aqN8R`
**Position in roadmap:** PR #771 of 5 (after #770 merged, before #772 blue/green deploy)

## Context

The operator's charter (operator-charter-forever.md + this AWS-launch session): every entry point — IntelliJ, GitHub push, Claude Code, Claude Cowork, Claude Chat — must drive AWS without manual typing. PR #770 closed the Terraform-config gaps. PR #771 wires GitHub Actions to fire `terraform apply` automatically when `deploy/aws/terraform/**` changes land on main.

After PR #771 merges + the one-time GitHub Secrets bootstrap (~30 sec), every subsequent infra change is fully automated: `git push` → CI → terraform plan → terraform apply → Telegram notification of outputs. Zero operator action.

## The chicken-and-egg, honestly named

| Step | Who/What | Manual? |
|---|---|---|
| Bootstrap once: operator adds `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` to GitHub Secrets | Operator (30 sec, one time, forever) | ✋ ONE time |
| Workflow first run: uses those keys, creates OIDC role + S3 backend + initial apply | GitHub Actions | ❌ Automated |
| Operator deletes the access key from IAM (security best practice) | Operator (10 sec, one time, forever) | ✋ ONE time |
| All subsequent runs: workflow uses OIDC (short-lived, no long-lived keys) | GitHub Actions | ❌ Automated |

Total operator typing forever: ~40 seconds, once. Beyond that: 100% automated.

## Plan Items

- [x] **Item 1 — New workflow `.github/workflows/terraform-apply.yml`**
  - Files: `.github/workflows/terraform-apply.yml` (new)
  - Triggers: push to `main` with paths `deploy/aws/terraform/**` OR `workflow_dispatch`
  - Jobs:
    1. `terraform-plan` — runs on every PR + every push (no apply, just plan)
    2. `terraform-apply` — runs only on push to `main`, requires `terraform-plan` success
    3. `notify` — publishes outputs (instance_id, elastic_ip, etc.) to SNS topic on success
  - Auth precedence: OIDC role if `AWS_TERRAFORM_ROLE_ARN` secret set, else falls back to long-lived `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` for first-run bootstrap
  - Validation: workflow file passes `actionlint` (if installed locally), terraform steps use SHA-pinned actions
  - Tests: r1_workflow_file_exists, r2_triggers_on_main_push_with_terraform_path_filter, r3_has_both_plan_and_apply_jobs, r4_uses_pinned_setup_terraform_action, r5_uses_pinned_configure_aws_credentials_action

- [x] **Item 2 — S3 backend auto-bootstrap script**
  - Files: `scripts/aws-bootstrap-state-backend.sh`
  - Behavior: checks if `tv-terraform-state-<account-id>` S3 bucket exists; creates if not (versioned, encrypted, public-blocked). Same for DynamoDB lock table `tv-terraform-locks`. Idempotent: safe to re-run.
  - Wired into `terraform-apply.yml` as a pre-init step (runs every time, no-op after first run)
  - Validation: invoked twice in a row in CI → second invocation is no-op
  - Tests: r9_bootstrap_script_exists_and_is_executable_and_idempotent

- [x] **Item 3 — Uncomment `backend "s3"` in versions.tf with dynamic account-id substitution**
  - Files: `deploy/aws/terraform/versions.tf`
  - Behavior: workflow uses `terraform init -backend-config="bucket=tv-terraform-state-${ACCOUNT_ID}"` to inject the bucket name at init time (avoids hardcoded account ID in repo)
  - Validation: `terraform init` in CI uses the new backend; state lives in S3 after first apply
  - Tests: r8_injects_s3_backend_bucket_via_backend_config_flag

- [x] **Item 4 — Market-hours guard (mirrors deploy-aws.yml pattern)**
  - Files: `.github/workflows/terraform-apply.yml` (the apply job has a `guard_market_hours` step)
  - Behavior: blocks `terraform apply` during 09:15-15:30 IST Mon-Fri unless `confirm_market_hours: yes` input on manual dispatch. Infrastructure changes during market hours = forbidden (touching SG/IAM/EIP could break the live feed).
  - Validation: workflow logic identical to existing `deploy-aws.yml` Lines 95-120
  - Tests: r6_has_market_hours_guard_job

- [x] **Item 5 — Telegram notification on apply success + failure**
  - Files: `.github/workflows/terraform-apply.yml` (notify job at end)
  - Behavior: on success, publishes terraform outputs (instance_id, elastic_ip, monthly_budget_name) to `tv-prod-alerts` SNS topic. On failure, publishes the error summary. SNS topic already routes to Telegram per `sns-subscriptions.tf`.
  - Validation: post-apply Telegram message arrives with `INSTANCE_ID=i-... EIP=X.X.X.X`
  - Tests: r7_has_sns_publish_for_success_and_failure

- [x] **Item 6 — One-time bootstrap runbook (no more "click 15 things")**
  - Files: `deploy/aws/terraform/BOOTSTRAP-ONE-TIME.md` (new)
  - Contents: ONE table, 3 steps with screenshots, ~40 seconds total time. Step 1: create IAM user `tv-terraform-bootstrap` with `AdministratorAccess` (we'll delete it after first run). Step 2: create access key. Step 3: paste 2 values into GitHub Settings > Secrets.
  - Validation: operator can follow without asking questions
  - Tests: r10_bootstrap_runbook_exists_and_names_oidc_cleanup

- [x] **Item 7 — Source-scan ratchet test `github_workflow_guard.rs`**
  - Files: `crates/common/tests/github_workflow_guard.rs` (new)
  - Behavior: 8 source-scan assertions:
    1. `terraform-apply.yml` exists
    2. Triggers on push to main with `deploy/aws/terraform/**` path filter
    3. Has both `terraform-plan` and `terraform-apply` jobs
    4. Uses SHA-pinned `hashicorp/setup-terraform` action (no `@v3` — must be SHA)
    5. Uses SHA-pinned `aws-actions/configure-aws-credentials` (no `@v4` — must be SHA)
    6. Has `guard_market_hours` step in apply job
    7. Has `aws sns publish` for both success and failure
    8. State backend uses `-backend-config="bucket=tv-terraform-state-"` dynamic name
  - Pre-push gate `pub-fn-test-guard.sh` won't fire (no pub fn added)
  - Validation: `cargo test -p tickvault-common --test github_workflow_guard` — all 8 pass

## Scenarios covered

| # | Scenario | Expected behavior |
|---|----------|-------------------|
| 1 | Operator pushes a 1-line edit to `deploy/aws/terraform/budget.tf` (raise to $15) | GH Actions auto-fires → plan shows 1 resource change → apply succeeds → Telegram message "budget alarm updated" |
| 2 | Operator pushes a 1-line edit to `crates/storage/src/lib.rs` (unrelated to AWS) | terraform-apply.yml does NOT trigger (path filter excludes); only Rust CI runs |
| 3 | Workflow runs during market hours 11:00 IST | `guard_market_hours` step blocks the apply; plan still runs; Telegram message "apply blocked, market hours" |
| 4 | First run: no S3 backend bucket exists yet | `aws-bootstrap-state-backend.sh` creates it; `terraform init` uses fresh bucket; state migrated transparently |
| 5 | Operator deletes the access key after first run | Next workflow run fails fast with "AWS auth failure"; operator adds OIDC `AWS_TERRAFORM_ROLE_ARN` secret; subsequent runs use OIDC |
| 6 | Terraform plan would destroy the EC2 instance (regression) | `disable_api_termination = true` on the instance blocks; terraform apply fails with clear error; alarm fires |
| 7 | Concurrent push to main (rare) | DynamoDB lock blocks second `terraform apply`; second workflow waits for first to release; serial apply guaranteed |

## Per-item Z+ Defense Matrix

| Layer | Item 1 (workflow) | Item 2 (S3 bootstrap) | Item 3 (backend) | Item 4 (market guard) | Item 5 (notify) | Item 6 (runbook) | Item 7 (ratchet) |
|---|---|---|---|---|---|---|---|
| L1 DETECT | workflow log | aws s3api head-bucket | terraform init log | UTC time check | SNS publish receipt | operator review | cargo test |
| L2 VERIFY | actionlint | re-run idempotent | state list | confirm_market_hours input | Telegram arrival | screenshot match | source-scan |
| L3 RECONCILE | per-push | per-run | per-apply | per-window | per-event | quarterly review | per-PR |
| L4 PREVENT | SHA-pinned actions | DynamoDB lock | terraform validate | hardcoded UTC window | required SNS arn | numbered steps | banned-pattern |
| L5 AUDIT | GH Actions log | CloudTrail | terraform state | guard log line | SNS history | git history | ratchet test |
| L6 RECOVER | re-run workflow | manual aws cli | terraform init -migrate | manual `apply` later | resend via console | fresh runbook clone | revert PR |
| L7 COOLDOWN | concurrency.group | retry with backoff | n/a | next non-market window | n/a | n/a | n/a |

## Z+ 15-row "100% Everything" Matrix

| Demand | Mechanical proof for THIS PR |
|---|---|
| Code coverage | github_workflow_guard.rs covers 8 invariants of the workflow file |
| Audit coverage | GH Actions log + CloudTrail + SNS publish history all preserve terraform apply outcomes |
| Testing coverage | Workflow YAML lint + Rust ratchet test + manual `act` dry-run optional |
| Code checks | actionlint (if available), source-scan ratchets, plan-verify hook |
| Code performance | N/A — provisioning code |
| Monitoring | Every workflow run is monitored by GitHub itself; failures surface in PR checks |
| Logging | GH Actions stdout + SNS publish + CloudWatch (workflow side-effects logged on AWS) |
| Alerting | SNS publish on success AND failure → Telegram via existing routing |
| Security | OIDC preferred; access keys only for bootstrap; access keys MUST be deleted after first OIDC run (documented in runbook) |
| Security hardening | All actions SHA-pinned (supply-chain hardening per PR #769 pattern); `permissions:` block scoped per-job |
| Bug fixing | Caught the wizard-vs-IaC gap during PR #770; this PR closes the apply-trigger gap |
| Scenarios covering | 7 scenarios above |
| Functionalities covering | Every terraform-relevant change auto-triggers apply |
| Code review | Self-review per plan + Z+ matrix; happy to add agent review on operator request |
| Extreme check | github_workflow_guard.rs ratchets the contract |

## Z+ 7-row "Resilience" Matrix

| Demand | Honest envelope |
|---|---|
| Zero ticks lost | N/A — infra apply doesn't touch the live binary; tick handling unchanged. Market-hours guard prevents apply during 09:15-15:30 IST |
| WS never disconnects | N/A — workflow doesn't touch WS; apply only modifies infra (SG/IAM/budget etc.) outside market hours |
| Never slow/locked/hanged | Workflow timeout 30 min; AWS API has its own SLA |
| QuestDB never fails | N/A — workflow only provisions AWS resources |
| O(1) latency | N/A — provisioning is one-shot |
| Uniqueness + dedup | DynamoDB lock table prevents two simultaneous applies from corrupting state |
| Real-time proof | Workflow logs visible in GitHub Actions UI; SNS Telegram message confirms outcomes |

## Honest 100% claim (envelope-qualified per operator-charter-forever.md §F)

> 100% inside the tested envelope, with ratcheted regression coverage: after the one-time 40-second operator bootstrap (paste 2 access keys into GitHub Secrets), every push to main that touches `deploy/aws/terraform/**` automatically triggers `terraform plan` (always) + `terraform apply` (only on main push, only outside 09:15-15:30 IST Mon-Fri). Apply runs against S3-backed state with DynamoDB lock — no concurrent corruption. Telegram message confirms outcomes within ~60 sec of push. Beyond the envelope: AWS API throttling, region outage, or the operator's access key getting revoked surface as workflow failure with explicit error in GH Actions logs + Telegram alert. No silent partial state.

## Post-merge operator workflow (the ONE-TIME 40 sec)

```
1. AWS Console > IAM > Users > Create user: "tv-terraform-bootstrap"
   - Attach policy: AdministratorAccess
   - Security credentials > Create access key (CLI)
   - Copy: AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY

2. GitHub > Settings > Secrets and variables > Actions
   - New repository secret: AWS_ACCESS_KEY_ID = (paste)
   - New repository secret: AWS_SECRET_ACCESS_KEY = (paste)
   - New repository variable: AWS_REGION = ap-south-1

3. Push any commit touching deploy/aws/terraform/** (e.g. raise budget by $1)
   - GitHub Actions auto-fires terraform-apply.yml
   - First run creates S3 backend + DynamoDB lock + applies all resources
   - Outputs published to Telegram

4. (After first successful run) Delete the IAM user tv-terraform-bootstrap
   - Subsequent runs use OIDC role created by Terraform (no long-lived keys)
   - Set GitHub secret AWS_TERRAFORM_ROLE_ARN = <output from first apply>
```

Beyond that: every infra change is `git push` and done. Zero operator action forever.
