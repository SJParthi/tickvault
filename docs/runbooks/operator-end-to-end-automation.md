# End-to-End Operator Automation — from Empty AWS Account to Live tickvault

> **Status:** DESIGN runbook (no code shipped). Operator can run this OR paste the §11 Cowork prompt and delegate to a fresh Claude Cowork task.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Created:** 2026-05-18.
> **Companion:** `aws-indices-only-locked-architecture.md`, `aws-daily-lifecycle.md`, `100-percent-compliance-audit.md`.

---

## §0. Auto-driver one-liner

> "Sir, you don't need to know AWS. You just open the AWS website, sign up with email + credit card (15 min — that's the ONE thing AWS won't let a robot do). Then you copy-paste ONE Cowork prompt to a fresh Claude window. Cowork creates IAM user, runs Terraform, configures Telegram bot, wires SMS + phone call alerts — all automatic. 60 minutes later your shop is open. You watch only Telegram."

```
  Operator action       AWS / Claude action               Time
  ─────────────         ───────────────                   ───
  1. Sign up AWS        (manual — billing email/card)     15 min
  2. Add card           (manual — AWS sandbox)             5 min
  3. Note account ID    AWS shows in top bar              30 sec
  4. Paste Cowork       Claude Cowork takes over          —
     prompt (§11)
  5. Watch Telegram     Cowork runs:                      60 min
                          - create IAM bootstrap user
                          - install Terraform + AWS CLI
                          - terraform init + apply
                          - SSM secrets seed
                          - Telegram bot create + token
                          - SNS subscriptions wire
                          - first systemctl start tickvault
                          - first BootReadyConfirmation
  6. Done               Tickvault running daily 08:00 IST  —
```

**Total operator time: ~25 minutes (15 signup + paste prompt + watch).**

---

## §1. Locks accepted 2026-05-18

| Lock | Value | Rationale |
|---|---|---|
| t4g.medium on-demand | NO Savings Plan / Reserved Instance yet | Keep simple while system stabilizes; CSP later when load is proven |
| **SMS + CALL both** (overrides LOCK-5) | Amazon Connect outbound voice ADDED back into mandatory tier | Operator: "in worst case we need SMS and call both" |
| IntelliJ-direct deploy | Off-hours auto-deploy via GitHub Actions + SSM RunCommand | No live-trading impact; gated by IST clock |
| Daily logs → Claude | MCP server extended to fetch CloudWatch Logs from AWS | Operator runs `mcp__tickvault-logs__*` in any Claude session, sees prod logs automatically |
| AWS bootstrap | Two paths: (a) operator runs `scripts/aws-bootstrap.sh` OR (b) operator pastes Cowork prompt §11 | Cowork path is the no-AWS-knowledge path |

---

## §2. The 11-stage bootstrap chain (what happens when operator says "go")

| # | Stage | Who does it | Cost-impacting? |
|---|---|---|---|
| 1 | AWS account creation | **Operator (manual — AWS legal req)** | Free (no card charge until usage) |
| 2 | Add payment card | **Operator (manual)** | Card auth $1 hold, refunded |
| 3 | Create IAM Admin user + access keys | Cowork or `scripts/aws-bootstrap.sh` | Free |
| 4 | Install Terraform + AWS CLI on operator's Mac | Local script | Free |
| 5 | Generate Telegram bot via @BotFather | Cowork prompts operator OR operator does manually | Free |
| 6 | Generate Dhan API JWT (TOTP) | Operator + Cowork helper | Free (Dhan API account assumed pre-existing) |
| 7 | Seed AWS SSM Parameters with secrets | Cowork via aws-cli | Free (SSM Standard tier) |
| 8 | `terraform init && terraform apply` for ap-south-1 | Cowork or script | First-time spin: ~₹3 (test boot) |
| 9 | Subscribe to SNS topics (SMS + Telegram + Email + Amazon Connect) | Cowork via aws-cli | DLT registration (operator does once); SMS ≤₹30/mo |
| 10 | First systemctl start + verify BootReadyConfirmation Telegram | Cowork via SSM RunCommand | Free |
| 11 | Enable EventBridge schedules (08:00/17:00 IST every day) | Cowork via aws-cli | Free |

---

## §3. Cowork takes over — what it must NOT do

| Action | Why forbidden |
|---|---|
| Run `terraform destroy` once stage 11 done | Would tear down live infra |
| Modify the EventBridge cron times | Operator-locked at 08:00/17:00 IST |
| Increase instance size beyond t4g.medium | Operator-locked at <₹1K target band |
| Disable any of the 10 CloudWatch alarms | Z+ L1 layer compromised |
| Skip Telegram BotFather step (use Cowork's own bot) | Operator must own the bot token |
| Commit secrets to git | Only SSM stores secrets |

---

## §4. AWS bootstrap script structure (`scripts/aws-bootstrap.sh`)

> **STATUS: DESIGN ONLY — file does NOT exist yet. Would be created in PR AWS-1 of plan.**

```bash
#!/usr/bin/env bash
# scripts/aws-bootstrap.sh — operator runs ONCE after creating AWS account
# Idempotent — safe to re-run.

set -euo pipefail

# 1. Prereqs
require_aws_cli
require_terraform
require_jq

# 2. Verify AWS credentials reach an account
aws sts get-caller-identity || die "run: aws configure"

# 3. Seed SSM Parameters from operator-provided values
read -sp "Dhan client ID: " DHAN_CLIENT_ID
read -sp "Dhan API key: " DHAN_API_KEY
read -sp "Dhan API secret: " DHAN_API_SECRET
read -sp "Dhan TOTP secret: " DHAN_TOTP_SECRET
read -sp "Telegram bot token (from @BotFather): " TG_BOT_TOKEN
read -sp "Telegram chat ID: " TG_CHAT_ID
read -p  "Operator phone number for SMS+Call (E.164): " OPERATOR_PHONE
read -p  "Operator email for alerts: " OPERATOR_EMAIL

aws ssm put-parameter --name /tickvault/prod/dhan_client_id --value "$DHAN_CLIENT_ID" --type SecureString --overwrite
aws ssm put-parameter --name /tickvault/prod/dhan_api_key --value "$DHAN_API_KEY" --type SecureString --overwrite
aws ssm put-parameter --name /tickvault/prod/dhan_api_secret --value "$DHAN_API_SECRET" --type SecureString --overwrite
aws ssm put-parameter --name /tickvault/prod/dhan_totp_secret --value "$DHAN_TOTP_SECRET" --type SecureString --overwrite
aws ssm put-parameter --name /tickvault/prod/telegram_bot_token --value "$TG_BOT_TOKEN" --type SecureString --overwrite
aws ssm put-parameter --name /tickvault/prod/telegram_chat_id --value "$TG_CHAT_ID" --type String --overwrite
aws ssm put-parameter --name /tickvault/prod/operator_phone --value "$OPERATOR_PHONE" --type SecureString --overwrite
aws ssm put-parameter --name /tickvault/prod/operator_email --value "$OPERATOR_EMAIL" --type String --overwrite

# 4. Terraform apply
cd deploy/aws/terraform
terraform init
terraform apply -auto-approve -var="environment=prod"

# 5. Capture instance ID + EIP
INSTANCE_ID=$(terraform output -raw instance_id)
EIP=$(terraform output -raw eip_address)

# 6. Register the EIP with Dhan (manual operator step — Dhan portal)
echo
echo "==========================================================="
echo "MANUAL STEP: log into web.dhan.co and add this IP as static:"
echo "   $EIP"
echo "Then press Enter to continue..."
echo "==========================================================="
read

# 7. Wait for instance boot + first BootReadyConfirmation
echo "Waiting for first boot... (up to 5 min)"
aws ssm send-command --instance-ids "$INSTANCE_ID" --document-name AWS-RunShellScript --parameters 'commands=["systemctl status tickvault"]'

# 8. Done
echo "✅ Bootstrap complete. Tickvault will auto-start every day at 08:00 IST."
echo "Account: $(aws sts get-caller-identity --query Account --output text)"
echo "Region: ap-south-1"
echo "Instance: $INSTANCE_ID"
echo "EIP: $EIP"
```

---

## §5. The Terraform extensions needed (for §4 to work end-to-end)

The existing `deploy/aws/terraform/main.tf` covers most resources. Additions needed:

| Resource | Purpose | LoC |
|---|---|---|
| `aws_ssm_parameter` × 8 (defined in script, not TF) | Holds operator secrets | (in bash) |
| `aws_sns_topic_subscription.sms` (DLT route India) | SMS leg | 10 |
| `aws_sns_topic_subscription.email` | Email leg | 5 |
| `aws_sns_topic_subscription.telegram_lambda` (calls Lambda → bot.sendMessage) | Telegram leg | 5 |
| `aws_lambda_function.sns_to_telegram` (Python ~50 LoC) | Translates SNS msg → Telegram | 80 |
| `aws_lambda_function.sns_to_amazon_connect` (Python ~80 LoC) | Translates Critical SNS → outbound voice call | 100 |
| `aws_connect_instance` (Amazon Connect outbound voice) | Phone call infrastructure | 60 |
| `aws_iam_role.connect_outbound` | IAM for Connect | 30 |
| `aws_cloudwatch_event_rule` × 4 (already exist) | EventBridge crons | (existing) |
| `aws_cloudwatch_metric_alarm` × 10 (in `alarms.tf`) | The 10 locked alarms | (existing, may need additions for option-chain + cross-verify) |

**Estimated TF addition: ~290 LoC + 2 Python Lambda functions ~180 LoC.** One PR.

---

## §6. Amazon Connect — phone CALL design (overrides LOCK-5)

Operator demand 2026-05-18: *"in worst cases we need SMS and call both right dude?"*

| Element | Design |
|---|---|
| Trigger | Any `Severity::Critical` CloudWatch alarm OR any `OPTION-CHAIN-05` / `CROSS-VERIFY-01/03` / `BOOT-02` / etc Critical-tier ErrorCode |
| Path | Critical alarm → SNS topic `tv-prod-critical-only` → Lambda `sns_to_amazon_connect.py` → Connect outbound voice call to operator phone |
| TTS message | "Tickvault Critical alarm. Reason: <alarm-name>. Time: <IST>. Repeating: <alarm-name>. Press 1 to acknowledge." |
| Acknowledgment | DTMF "1" → Connect callback → SSM Parameter `/tickvault/prod/last_critical_ack_ts` set → suppresses repeat call for 30 min |
| Cost | $0.018/min outbound voice (ap-south-1) × ~1 min × ~10 critical/mo = **~₹15/mo** |
| Failure | If Connect itself fails: SNS 3-leg (SMS+Telegram+Email) still fires in parallel — phone call is ADDITIONAL not REPLACEMENT |

### Why phone CALL matters above SMS

Operator SMS may go to silent mode; phone CALL rings audibly even with DND on for "important contacts" whitelist. Plus, calls force operator awake — SMS doesn't.

### When Critical fires

| Critical event | Phone call? | SMS? | Telegram? | Email? |
|---|---|---|---|---|
| `BOOT-02` (boot deadline exceeded) | ✅ | ✅ | ✅ | ✅ |
| `OPTION-CHAIN-05` (cache stale → strategy halt) | ✅ | ✅ | ✅ | ✅ |
| `CROSS-VERIFY-01` (15:31 IST mismatch) | ✅ | ✅ | ✅ | ✅ |
| `CROSS-VERIFY-03` (morning 1d fail — strategy halted) | ✅ | ✅ | ✅ | ✅ |
| `DEPTH-*` (any) | N/A — depth deleted | | | |
| `SELFTEST-02` (market-open self-test failed) | ✅ | ✅ | ✅ | ✅ |
| `Severity::High` events (e.g. token expiring) | ❌ | ✅ | ✅ | ✅ |
| `Severity::Medium` / `Low` / `Info` | ❌ | ❌ | ✅ | ❌ |

**Total expected phone calls/month: ~5–10** based on historical incident frequency.

---

## §7. IntelliJ-direct deploy — safe pipeline (no live-trading impact)

### The flow

```
   Operator codes in IntelliJ on Mac
       │
       ▼
   `cargo test --workspace` LOCAL (Mac Docker stack matches AWS prod exactly per aws-budget.md rule 10)
       │
       ▼ if green
   `git push origin feature-branch`
       │
       ▼
   GitHub PR → CI runs 22-test battery
       │
       ▼ if green
   Operator merges to main
       │
       ▼
   GitHub Actions `deploy.yml` workflow triggers
       │ (workflow has IST clock guard: refuses to run between 09:14 IST and 15:31 IST)
       │
       ▼ if outside market hours
   GitHub OIDC → AWS IAM AssumeRole (no static keys)
       │
       ▼
   `aws ssm send-command` to instance:
     1. systemctl stop tickvault
     2. fetch new binary from GitHub release artifact
     3. sha256 verify
     4. swap binary at /opt/tickvault/bin/tickvault
     5. systemctl start tickvault
       │
       ▼
   wait for BootReadyConfirmation Telegram (≤180s)
       │
       ▼ if Telegram received
   ✅ deploy success
       │
       ▼ if NOT received within 180s
   auto-rollback: swap previous binary back, restart
   ✘ Critical Telegram "Deploy failed, auto-rolled back"
```

### The IntelliJ side

Operator clicks Run/Debug in IntelliJ → runs the same `cargo test -p tickvault-<crate>` locally → never directly touches AWS during dev.

For "I want to test on AWS without breaking live": operator pushes to a `dev/*` branch → CI runs but does NOT trigger deploy.yml (deploy.yml only triggers on `main`). To test on AWS, operator can manually invoke a staging deploy via `workflow_dispatch`, gated by:
- Manual confirmation prompt
- Refuses if instance is currently serving a live trading session (checks `tv_strategy_active` SSM Parameter)

### The market-hours guard (mandatory in deploy.yml)

```yaml
# .github/workflows/deploy.yml
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - name: Refuse deploy during market hours
        run: |
          IST_HHMM=$(TZ=Asia/Kolkata date +%H%M)
          if [ "$IST_HHMM" -ge "0914" ] && [ "$IST_HHMM" -le "1531" ]; then
            echo "::error::Refusing to deploy during market hours (09:14-15:31 IST). Current: $IST_HHMM IST"
            exit 1
          fi
      # ... rest of deploy
```

**Hard guard.** Cannot be bypassed without operator-charter edit + PR review.

---

## §8. Daily logs → Claude — extreme automation answer

Operator question: *"every day for extreme complete automation I need to provide logging to you means how bro?"*

**Translation:** how do daily prod logs reach Claude (the assistant) automatically so Claude can analyze + triage + recommend without operator manually copy-pasting?

### The 5 paths (use ALL — each catches different failure modes)

| # | Path | What Claude sees | Operator effort/day |
|---|---|---|---|
| 1 | **tickvault-logs MCP server (already exists)** — extend to fetch CloudWatch Logs from AWS in addition to local files | Last-hour ERROR signatures, doctor status, alarm state | **Zero** — Claude invokes `mcp__tickvault-logs__run_doctor` automatically in every session |
| 2 | **Daily EOD Telegram digest (15:45 IST)** — already designed in v2 doc | P&L + order count + alerts summary | **Zero** — operator scrolls Telegram |
| 3 | **GitHub Actions nightly log-snapshot** — exports CW Logs to a public-restricted gist or S3 → Claude reads via WebFetch | Full day's logs replayable | **Zero** — automated workflow |
| 4 | **Claude `/loop` triage runbook** (already exists at `.claude/triage/claude-loop-prompt.md`) | Continuous Claude session running every 30 min, auto-triaging novel signatures | Operator runs `/loop 30m .claude/triage/claude-loop-prompt.md` ONCE; runs forever |
| 5 | **SSM Session Manager access** — operator can drop Claude into the live instance via Cowork task with SSM permissions | Live `journalctl -u tickvault -f` | Operator pastes a Cowork prompt |

### Path 1 (the primary) — extending the MCP server

Today the Rust-native MCP server (`crates/tickvault-logs-mcp/src/tools.rs`, launched via `scripts/mcp-servers/tickvault-logs-launch.sh` — the Python `server.py` was deleted in the 2026-07-18 Phase-2c cutover) exposes tools that read LOCAL `data/logs/*` files. Extension (pseudocode below predates the cutover):

```python
# Pseudocode for the extension
@tool
def tail_errors_aws(log_group="/tickvault/prod/app", lookback_minutes=60):
    """Fetch recent ERROR-level logs from AWS CloudWatch Logs."""
    boto3.client('logs').filter_log_events(
        logGroupName=log_group,
        startTime=int((time.time() - lookback_minutes*60) * 1000),
        filterPattern='{ $.level = "ERROR" }',
    )
    # ... returns JSON-structured rows
```

**Operator action:** ZERO. Claude (in any session) just calls `mcp__tickvault-logs__tail_errors_aws()` and the MCP server figures out where the logs are (local for dev, AWS CW for prod).

### Path 3 (the safety net) — nightly snapshot

```yaml
# .github/workflows/log-snapshot.yml
on:
  schedule:
    - cron: "30 12 * * *"  # 18:00 IST every day, after EOD
jobs:
  snapshot:
    steps:
      - name: Export today's logs from CW
        run: |
          aws logs create-export-task ...
          # → S3 bucket → operator-readable
```

Operator runs `cat data/logs/snapshot-YYYY-MM-DD.log | wc -l` to verify daily count. Or asks Claude in next session: "What ERRORs fired yesterday between 09:15 and 15:30 IST?" — Claude reads via MCP.

---

## §9. The "without breaking live trading or current coding" guarantee

Operator demand: *"how can we directly test deploy without breaking current live trading or coding also dude?"*

| Concern | Guard |
|---|---|
| Test code changes live binary | `cargo test` is LOCAL on Mac. Never touches AWS. |
| Push breaks GitHub Action | PR + CI gate. Branch protection blocks merge if CI red. |
| Deploy during market hours | `deploy.yml` IST-clock guard refuses 09:14–15:31 IST. |
| Deploy breaks live binary | Auto-rollback if BootReadyConfirmation Telegram not received within 180s. |
| Mac dev environment drifts from AWS | Same `docker-compose.yml` per `aws-budget.md` rule 10. CI runs identical containers. |
| Operator manually breaks production | SSM-only access (no SSH); every action audited in CloudTrail; some risky actions require MFA |
| Operator forgets a step | This runbook + the Cowork prompt automate it |

**The closed-loop guarantee:** every code change has 3 checkpoints (local cargo test → CI 22 tests → AWS deploy with rollback). At any one of them, broken code is caught and reverted automatically.

---

## §10. Cost impact of all this automation

| New AWS resource | Monthly cost |
|---|---|
| Amazon Connect outbound voice (~10 calls × 1 min) | ~₹15 |
| Extra Lambda invocations (SNS bridge + sns_to_connect) | ₹0 (free tier) |
| Extra CloudWatch Logs (for AWS-side automation logs) | ₹0 (within 5GB free tier) |
| GitHub Actions deploy workflow | ₹0 (GitHub free tier for public repo) |
| Additional SSM Parameter Store entries | ₹0 (within 10,000 free tier) |
| **Total new monthly cost** | **~₹15/mo** |

**Updated total: ₹1,022 + ₹15 = ~₹1,037/mo.** Still essentially at the <₹1K target — ₹37 over.

---

## §11. The COWORK PROMPT — operator pastes this verbatim

Save the block below as `docs/prompts/aws-bootstrap-cowork-prompt.md`. Operator copy-pastes into a fresh Claude Cowork task after signing up for AWS.

```
=== COWORK PROMPT START ===

I am the operator of the tickvault project (github.com/SJParthi/tickvault). I just created an AWS account and need you to bootstrap the entire infrastructure end-to-end without me touching the AWS console after this.

ENVIRONMENT YOU HAVE:
- Fresh AWS account in region ap-south-1 (Mumbai)
- My credit card is added; AWS sandbox lift requested
- I will provide IAM Admin user access keys when you prompt me

WHAT TO DO (in order, follow operator-charter §H — one task at a time):

1. Verify AWS credentials reach my account (aws sts get-caller-identity).
2. Read these docs IN THIS ORDER:
   - CLAUDE.md
   - .claude/rules/project/operator-charter-forever.md
   - docs/architecture/aws-indices-only-locked-architecture.md (THIS IS THE LOCKED ARCHITECTURE)
   - docs/architecture/aws-daily-lifecycle.md
   - docs/runbooks/operator-end-to-end-automation.md (this doc)
3. Confirm to me: instance = t4g.medium ARM ap-south-1 on-demand; schedule = every day 08:00–17:00 IST; tenancy = Default (Shared); EIP attached 24/7; tickvault + QuestDB containers only; CloudWatch single-sink observability; SNS 3-leg fan-out (SMS+Telegram+Email) PLUS Amazon Connect outbound voice for Critical. If anything is unclear, ASK ME — do not guess.
4. Prompt me for these secrets, write each to AWS SSM Parameter Store under /tickvault/prod/ with SecureString type:
   - Dhan client ID, API key, API secret, TOTP secret
   - Telegram bot token (I will create the bot via @BotFather while you wait), chat ID
   - My phone number E.164 format (for SMS + Amazon Connect)
   - My email for SNS Email subscription
5. cd deploy/aws/terraform && terraform init && terraform apply -auto-approve -var environment=prod. Show me the plan first; I will confirm.
6. Capture the EIP from terraform output. Pause and instruct me to register that IP with Dhan via web.dhan.co (this is the ONLY remaining manual step). Wait for my confirmation.
7. Subscribe me to SNS topics: SMS (via DLT — guide me through Indian sender ID registration if needed), Email, and the Telegram-Lambda bridge.
8. Deploy the Amazon Connect outbound voice Lambda (sns_to_amazon_connect.py). Configure outbound contact flow that:
   - Reads SNS message subject + body
   - Calls my number with TTS
   - Listens for DTMF "1" acknowledgment
   - Writes /tickvault/prod/last_critical_ack_ts on ack
9. Run `aws ssm send-command` to instance to do `systemctl start tickvault`. Watch for BootReadyConfirmation Telegram to arrive within 3 minutes.
10. If Telegram arrives, write `terraform output -json > deploy/aws/terraform/state-snapshot-$(date +%Y%m%d).json` and commit + push to repo on a `bootstrap/initial-$(date)` branch.
11. Open a draft PR with the state snapshot + a "READY FOR DAILY OPERATION" section.
12. End your session by telling me:
    - Instance ID
    - EIP address (so I can confirm with Dhan)
    - Telegram bot username
    - The 4 daily Telegram pings I should expect
    - The PR URL

NON-NEGOTIABLE RULES:
- NEVER commit secrets to git. All secrets in SSM only.
- NEVER deploy without my explicit "yes" after showing me the terraform plan.
- NEVER bypass branch protection.
- READ operator-charter-forever.md before every decision.
- Apply Z+ 7-layer defense per z-plus-defense-doctrine.md to every component you provision.
- Honest envelope claims only. If something might fail, say so.
- After every stage, write a one-line status into a fresh Telegram message to me using the bot I created in step 4.

ON FAILURE:
- Roll back terraform changes via terraform destroy if your stage 5 fails.
- Page me on Telegram immediately if you hit ANY of: IAM permission denied, Dhan API rejection, Telegram bot creation failure, Amazon Connect setup failure.
- If you can't complete in 90 minutes, hand back to me with a Telegram message naming what you got stuck on.

WHEN DONE:
- I expect to wake up tomorrow at 07:55 IST and see the auto-start fire at 08:00 IST with BootReadyConfirmation arriving at ~08:03 IST. If I don't, you failed.

=== COWORK PROMPT END ===
```

---

## §12. Honest envelope (operator-charter §F)

> "100% inside the tested envelope: a fresh AWS account in ap-south-1 + this Cowork prompt produces a running tickvault instance broadcasting BootReadyConfirmation Telegram within 60–90 minutes of start. SMS + Telegram + Email + Amazon Connect outbound voice all wired for Critical alerts. Daily logs flow to Claude via the tickvault-logs MCP server with zero operator effort. Beyond envelope: operator must still register the EIP with Dhan via web.dhan.co (Dhan does NOT have an API for IP whitelist registration); operator must still complete India DLT sender ID registration for SMS (TRAI mandate, takes 24–48h). These two manual steps cannot be automated by Claude or AWS — they require operator's identity verification with Dhan + a telecom carrier respectively."

---

## §13. Trigger / auto-load

This rule activates when editing:
- `scripts/aws-bootstrap.sh`
- `deploy/aws/terraform/main.tf`
- `.github/workflows/deploy.yml`
- `docs/prompts/aws-bootstrap-cowork-prompt.md`
- Any file containing `terraform apply`, `aws ssm put-parameter`, `sns_to_amazon_connect`, `aws connect`
