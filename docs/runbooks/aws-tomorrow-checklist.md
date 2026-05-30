# AWS Go-Live (Data-Pull) — Tomorrow-Evening Checklist

> **Goal:** the tickvault app running on the AWS m8g.large box, authenticating
> to Dhan, fetching the daily universe, and storing ticks/option-chain to
> QuestDB — in **staging (sandbox / dry_run) mode, NO real orders** (operator
> lock: 3-month data-pull only).
> **You run these on your Mac;** Claude troubleshoots each output live.
> **Run off-market** (evening / weekend) so the deploy market-hours guard passes.

---

## How the deploy actually works (so the steps make sense)

The AWS box does **NOT** build Rust. The pipeline is:

```
git push to crates/** or deploy/systemd/**  (e.g. PR #898 merge)
        │
        ▼
GitHub Actions  deploy-aws.yml
  ├─ build job (ubuntu-24.04-arm): cargo build --release --bin tickvault  (ARM64)
  ├─ uploads binary → s3://tv-prod-cold/deploys/<sha>/tickvault
  └─ deploy job → SSM command on the box:
        download binary → run smoke_test (must pass <30s) → atomic swap
        → systemctl daemon-reload → systemctl restart tickvault → verify active
```

So **deploying the app = merging a PR + letting the workflow run.** The only
manual things are: (a) one-time bootstrap secrets in GitHub, (b) the
`/tickvault/staging/*` SSM secrets, (c) confirming.

---

## ⛔ Blocker to clear FIRST — GitHub deploy secrets

The deploy workflow is **skipped** unless these GitHub repo secrets exist
(Settings → Secrets and variables → Actions):

| Secret | Value | How |
|---|---|---|
| `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` | bootstrap IAM keys | already done if `make aws-deploy` ran; else `aws-one-shot-bootstrap.sh` |
| `EC2_INSTANCE_ID` | `i-0b956d0209231a48b` (your tv-prod-app) | add manually |
| `AWS_TERRAFORM_ROLE_ARN` | OIDC role (optional, after first apply) | from `terraform output` |

**Check:** GitHub → repo → Actions tab → has `deploy-aws` ever run green? If the
first Terraform apply went through `make aws-deploy`, the AWS keys are already
set; you likely just need to **add `EC2_INSTANCE_ID = i-0b956d0209231a48b`.**

---

## Step 1 — let PR #898 merge (the env-wiring fix)

PR #898 routes the AWS box to **staging** (sandbox/dry_run) and fixes the
config/SSM env wiring. Auto-merges on green CI. Confirm it's on `main`:
```bash
cd ~/tickvault && git pull origin main && git log --oneline -3
# expect the fix(config): TV_ENVIRONMENT ... commit in main
```

## Step 2 — seed the STAGING secrets into SSM (~10 min)

The box runs `TV_ENVIRONMENT=staging` → reads `/tickvault/staging/*`. Seed them
(copy values from your existing `/tickvault/dev/*` via SSM console → Show):
```bash
cd ~/tickvault
./scripts/aws-seed-ssm-parameters.sh --env staging --dry-run   # preview
./scripts/aws-seed-ssm-parameters.sh --env staging             # real (silent prompts)
```
Seeds: dhan client-id/client-secret/totp-secret, questdb pg-user/pg-password,
telegram bot-token/chat-id, api bearer-token. `network/static-ip` → press Enter
(EIP off). sandbox/sns → skip.

> Why staging not prod: `production.toml` arms real money (dry_run=false, real
> api.dhan.co, EIP-required). Staging = sandbox, dry_run=true, can NEVER flip
> live. Correct for the data-pull phase.

## Step 3 — trigger the deploy (binary build + ship + restart)

The #898 merge (touches `deploy/systemd/**` + `crates/**`) **auto-fires**
`deploy-aws.yml` IF the GitHub secrets from the blocker section are set.

- **Watch it:** GitHub → Actions → `deploy-aws` run → should go build → deploy → green.
- **If it didn't fire** (or you want to force it): GitHub → Actions →
  `deploy-aws` → "Run workflow" (manual dispatch), or:
  ```bash
  gh workflow run deploy-aws.yml --repo SJParthi/tickvault --ref main
  ```

The deploy job downloads the ARM64 binary, runs the 30s smoke test, atomic-swaps
it into `/opt/tickvault/bin/tickvault`, and `systemctl restart tickvault`.

## Step 4 — verify the app is alive on the box

```bash
# tail the app log via SSM (no SSH needed):
make aws-ssm-command
# or open a shell:
make aws-ssm
#   inside the box:
#     systemctl status tickvault --no-pager
#     journalctl -u tickvault -n 80 --no-pager
#     cat /opt/tickvault/data/logs/errors.summary.md   # want: "Zero ERROR-level events"
```

**Healthy looks like (same as your Mac boot today):**
- `initial authentication successful` (Dhan JWT via SSM→TOTP)
- `daily-universe boot complete ... universe_size: 331`
- `option-chain snapshot persisted to QuestDB`
- `system ready ... mode: ... api_port: 3001`
- secrets path shows `/tickvault/staging/...` (NOT dev) ✅

## Step 5 — confirm the auto-schedule

The box auto-starts **weekday 08:30 IST** and stops **16:30 IST** (EventBridge).
Monday 09:15 IST it captures the live feed automatically. Nothing to do — just
confirm the two EventBridge rules exist:
```bash
aws events list-rules --region ap-south-1 --query "Rules[?contains(Name,'tv-prod')].Name"
```

---

## What is intentionally OFF (don't "fix" these)

| Thing | State | Why |
|---|---|---|
| Real orders | OFF (`dry_run=true`, sandbox) | operator lock — 3-month data-pull only |
| Elastic IP | OFF (`enable_eip=false`) | not needed without orders; saves ₹430/mo |
| `production.toml` | dormant, never loaded | arms real money; only loads on `TV_ENVIRONMENT=prod` |
| Weekend run | OFF unless manual start | weekday-only schedule |

---

## If something errors — paste the output to Claude

| Symptom | Runbook |
|---|---|
| deploy-aws workflow skipped | bootstrap secrets missing → blocker section above |
| smoke_test fails in deploy | `journalctl` on box; usually a config/SSM gap |
| app exit-loops after restart | `docs/runbooks/aws-tickvault-exit-loop.md` (often missing `/tickvault/staging/*` secret) |
| auth fails on box | confirm staging secrets seeded (Step 2); check totp-secret |
| EventBridge didn't start box | `docs/runbooks/aws-eventbridge-rule-missing.md` |
| instance capacity error | `docs/runbooks/aws-capacity-error.md` |

**Critical-path order: clear the GitHub-secrets blocker → merge #898 → seed
staging SSM → let deploy run → verify. That's the whole evening.**
