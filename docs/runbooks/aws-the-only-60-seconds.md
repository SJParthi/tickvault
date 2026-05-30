# AWS — The ONLY 60 Seconds You Will Ever Spend

> **Read this first.** Everything about running tickvault on AWS is automatic —
> provisioning, install, deploy, secrets, start/stop schedule, upgrades,
> monitoring, alerts. **Except two things that NO software on earth can do for
> you**, because they are AWS's legal + security root:
>
> 1. **Create the AWS account** (needs a human + a credit card).
> 2. **Mint the very first login key** (~20 sec of clicking).
>
> That's it. ~60 seconds, ONCE, forever. This page is those 60 seconds, in
> plain English, assuming you know nothing about AWS. After the one command at
> the end, you never touch AWS again — the robot does the rest.

---

## Why these two steps can't be automated (30-second honesty)

To make AWS do anything by software, you need a *key*. To create a key by
software, you need a key. Someone has to make the **first** key by hand — there
is no way around the chicken-and-egg. And AWS won't even give you a key until a
human creates an account with a real payment method (that's the law, anti-fraud).
After the first key exists, our robot immediately swaps it for an auto-rotating
one and you're done forever.

**Analogy:** to get a bank ATM card you must walk into the branch once with your
ID. After that, the ATM does everything 24/7. You can't get the *first* card by
ATM. AWS is the same.

---

## STEP 1 — Create the AWS account (~3 min, one time)

1. Open **https://aws.amazon.com/** → click **Create an AWS Account** (top right).
2. Enter: an **email**, a **password**, an **account name** (type: `tv-prod`).
3. Enter your **name, address, phone**.
4. Enter a **credit/debit card** (Indian card is fine). AWS may charge ₹2 to
   verify, then refund it. *Our setup is budgeted at ~₹2,058/month and has an
   automatic kill-switch at $25 — it cannot run away on cost.*
5. Verify your **phone** (they send an OTP).
6. Choose the **Basic (free) support plan**.
7. Done. You can now **sign in to the AWS Console**.

✅ **You never need to understand anything inside AWS.** You just needed the account to exist.

---

## STEP 2 — Mint the ONE key (~20 sec, one time)

Signed in to the AWS Console:

1. In the top search bar, type **IAM** → click it.
2. Left menu → **Users** → **Create user**.
   - User name: `tv-bootstrap` → **Next**.
   - Permissions: **Attach policies directly** → tick **AdministratorAccess** → **Next** → **Create user**.
3. Click the new **`tv-bootstrap`** user → **Security credentials** tab →
   **Create access key** → choose **Command Line Interface (CLI)** → tick the
   confirm box → **Next** → **Create access key**.
4. You now see **Access key** + **Secret access key**. **Copy both** (or
   "Download .csv"). *This is the only time AWS shows the secret.*

✅ That's the last click you'll ever make in the AWS Console.

---

## STEP 3 — Hand the key to the robot (1 command, one time)

On your Mac (one-time tools: `brew install awscli gh jq` and `gh auth login`):

```bash
cd ~/tickvault
git pull origin main

aws configure
#   AWS Access Key ID:     <paste from Step 2>
#   AWS Secret Access Key: <paste from Step 2>
#   Default region name:   ap-south-1
#   Default output format: json

export TF_VAR_operator_email=sjparthi93@gmail.com
export TF_VAR_operator_cidr=$(curl -s ifconfig.me)/32

make aws-deploy
```

**That single `make aws-deploy` is the last thing you do.** From here the robot:

- builds the entire AWS infrastructure (server, network, storage, alarms, budget)
- installs Docker + tickvault + the database on the server automatically
- starts/stops the server on the weekday market schedule automatically
- swaps your hand-made key for an auto-rotating one, then deletes the hand-made key
- monitors everything and pings your Telegram if anything is wrong

---

## STEP 4 — Two clicks of follow-up (≤30 sec, the robot tells you when)

1. **Confirm the alert email.** AWS sends `sjparthi93@gmail.com` a
   *"Subscription Confirmation"* — click the link so alerts can reach you.
2. **Give the robot your secrets once.** Run `./scripts/aws-seed-ssm-parameters.sh prod`
   and paste your Dhan client-id / secret / TOTP + Telegram token when prompted.
   (These are YOUR private credentials — by design only you can supply them; AWS
   stores them encrypted and the app reads them automatically every boot.)

> Want even Step 4.2 automated? Claude can add a mode that reads all secrets from
> one encrypted file so there are zero prompts — ask for it.

---

## After that — ZERO human effort, forever

| Thing | Who does it |
|---|---|
| Provision the server + network + storage | 🤖 robot (`terraform apply` via GitHub) |
| Install Docker + app + database | 🤖 robot (first-boot script) |
| Authenticate to Dhan every day | 🤖 robot (token auto-refresh) |
| Start 08:30 IST / stop 16:30 IST, weekdays | 🤖 robot (EventBridge cron) |
| **Upgrade the instance** (e.g. bigger box) | 🤖 robot (`scripts/aws-upgrade-instance.sh`) |
| Monitor + alert to Telegram | 🤖 robot (CloudWatch → SNS) |
| Keep cost under budget | 🤖 robot (auto kill-switch at $25) |
| Create the AWS account | 👤 you, once (Step 1) |
| Make the first key | 👤 you, once (Step 2) |

**Total human effort across the entire lifetime of the system: the ~60 seconds
on this page.**

---

## Before you place a REAL order (not now — only when you go live)

Two safety locks stay ON until you deliberately flip them:

- **Static IP (Elastic IP) is OFF** during the data-pull phase. Dhan requires a
  fixed IP for live orders — Claude flips `enable_eip=true`, re-applies, and
  re-registers it with Dhan. **Do this ≥ 7 days before going live** (Dhan has a
  7-day cooldown on IP changes).
- **`dry_run=true`** — no real orders fire until you explicitly turn it off.

Ask Claude when you're ready; it's a guided change, still no AWS-console clicking.

---

## Full detail

The complete step-by-step (with the security cleanup, verification, and a
symptom→runbook map) is in **`docs/runbooks/aws-go-live-guide.md`**.
