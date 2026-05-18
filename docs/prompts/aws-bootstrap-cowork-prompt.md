# AWS Bootstrap — Claude Cowork Prompt

> **Status:** Canonical operator-pasteable prompt for AWS-from-scratch bootstrap.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion:** `docs/runbooks/operator-end-to-end-automation.md` §11 (this prompt's source-of-truth).
> **How to use:** copy the block below (between the `=== COWORK PROMPT START ===` and `=== COWORK PROMPT END ===` markers) verbatim and paste into a fresh Claude Cowork task window after creating an AWS account.

---

## §0. Pre-paste checklist (operator does manually first)

| Step | Why |
|---|---|
| 1. Sign up for AWS at https://aws.amazon.com — email + credit card | Cowork cannot create AWS accounts |
| 2. Wait for "sandbox lifted" email (~10 min) | Else most regions are restricted |
| 3. Create one IAM Admin user via AWS console → generate access keys | Cowork needs these credentials |
| 4. Note your AWS account ID (top-right corner of console) | For Cowork to verify |

---

## §1. The prompt — paste this verbatim

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

## §2. What to expect during the 60-90 min Cowork session

| Minute | Cowork action | Operator action |
|---|---|---|
| 0 | Reads CLAUDE.md + locked architecture doc | Confirms credentials |
| 5 | Verifies AWS access | Provides SSM secrets |
| 15 | Shows Terraform plan | Approves with "yes" |
| 25 | `terraform apply` runs | (waits) |
| 35 | EIP captured | Registers EIP with Dhan via web.dhan.co |
| 45 | SNS subscriptions wired | Confirms SMS DLT registration if needed |
| 55 | Amazon Connect Lambda deployed | (waits) |
| 65 | `systemctl start tickvault` | Watches for Telegram |
| 75 | BootReadyConfirmation arrives | ✅ Success |
| 85 | Draft PR opened with state snapshot | Reviews PR |

---

## §3. After bootstrap — daily ops

Per `aws-indices-only-locked-architecture.md` §17, the 4 daily positive Telegrams you should expect:

| IST | Telegram | Meaning |
|---|---|---|
| 08:03 | ✅ `tickvault started` | Auto-start + boot succeeded |
| 09:15:30 | ✅ `Streaming live` | Market opened, feed flowing |
| 15:31 | ✅ `Cross-match OK` | Same-day data integrity proven |
| 15:45 | EOD digest | P&L + order count + alarms |

**If any of these 4 is missing on a trading day → investigate.** See `docs/runbooks/aws-disaster-recovery.md` and the 7 `aws-*` worst-case runbooks for triage.

---

## §4. Trigger / auto-load

This file is the canonical extraction of the Cowork prompt. Activates when editing:
- `docs/runbooks/operator-end-to-end-automation.md` §11 (the source-of-truth)
- This file (the extraction)
- Any AWS bootstrap automation script
