# 🎯 Tickvault Operator Console — EVERYTHING IN ONE PAGE

> **BOOKMARK THIS PAGE.** Every URL clickable. Live data on every link.
> **Account:** `208384284948` · **Region:** `ap-south-1` (Mumbai) · **Instance:** `i-0b956d0209231a48b` · **Box hostname:** `tv-prod-app` · **Elastic IP:** `13.205.212.203`

---

## ⭐ THE 5 LINKS YOU OPEN EVERY DAY

1. 📊 **[LIVE DASHBOARD →](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#dashboards/dashboard/tv-prod-operator)** — every metric in one view
2. 📜 **[LIVE LOGS →](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Ftickvault$252Fprod$252Fapp/log-events)** — streaming, click "Start tailing"
3. 🚨 **[ALARMS →](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarmFilter=ALARM)** — red = action needed
4. 🚀 **[DEPLOYS →](https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml)** — every deploy run
5. 💰 **[BILLING →](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/costexplorer)** — cost vs ₹2000 target

---

# 📊 DASHBOARDS (Real-Time, Graphs)

| Dashboard | URL |
|---|---|
| **Operator Overview** (custom, all metrics) | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#dashboards/dashboard/tv-prod-operator) |
| **All Dashboards** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#dashboards:) |
| **CloudWatch Overview** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#home:) |
| **EC2 Instance Detail** | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#InstanceDetails:instanceId=i-0b956d0209231a48b) |
| **EC2 Instance Metrics** | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#Instance:instanceId=i-0b956d0209231a48b;tabId=monitoring) |
| **EC2 Status Checks** | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#Instance:instanceId=i-0b956d0209231a48b;tabId=status) |
| **Service Health (AWS region status)** | [open](https://health.aws.amazon.com/health/status) |

---

# 📜 LOGS (Real-Time Streams)

## App logs
| Log group | URL |
|---|---|
| **App logs `/tickvault/prod/app`** | [open log group](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Ftickvault$252Fprod$252Fapp) · [LIVE TAIL](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:live-tail) |
| **All log groups** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups) |
| **Logs Insights (query language)** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:logs-insights) |

## Lambda logs
| Function | Logs |
|---|---|
| `tv-prod-telegram-webhook` | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Faws$252Flambda$252Ftv-prod-telegram-webhook) |
| `tv-prod-budget-killswitch` | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Faws$252Flambda$252Ftv-prod-budget-killswitch) |
| `tv-prod-daily-budget-digest` (after PR #934) | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Faws$252Flambda$252Ftv-prod-daily-budget-digest) |
| `tv-prod-hard-stop-guard` (after PR #934) | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Faws$252Flambda$252Ftv-prod-hard-stop-guard) |

## Pre-built log queries (Logs Insights — copy-paste)
- **Last 100 ERRORs:** `fields @timestamp, @message | filter @message like /ERROR/ | sort @timestamp desc | limit 100`
- **Boot events:** `fields @timestamp, @message | filter @message like /BOOT-|tickvault started|Auth OK/`
- **Tick rate (per minute):** `fields @timestamp | filter @message like /tick/ | stats count() by bin(1m)`

---

# 🚨 ALARMS

| What | URL |
|---|---|
| **All Alarms** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:) |
| **🔴 FIRING ONLY** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarmFilter=ALARM) |
| **⚠️ Insufficient data** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarmFilter=INSUFFICIENT_DATA) |
| **✅ OK** | [open](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarmFilter=OK) |

### Individual alarms (click to see graph + history)
- [`tv-prod-clock-skew-high`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-clock-skew-high)
- [`tv-prod-cpu-high-5min`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-cpu-high-5min)
- [`tv-prod-disk-used-high`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-disk-used-high)
- [`tv-prod-dlq-ticks`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-dlq-ticks)
- [`tv-prod-questdb-disconnected`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-questdb-disconnected)
- [`tv-prod-realtime-guarantee-critical`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-realtime-guarantee-critical)
- [`tv-prod-spill-dropped`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-spill-dropped)
- [`tv-prod-tick-gap-instruments-silent`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-tick-gap-instruments-silent)
- [`tv-prod-token-remaining-low`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-token-remaining-low)
- [`tv-prod-ws-pool-all-dead`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarm/tv-prod-ws-pool-all-dead)

---

# 🖥️ BOX ACCESS (browser SSH, no key needed)

| What | URL |
|---|---|
| **Session Manager (browser terminal)** | [open](https://ap-south-1.console.aws.amazon.com/systems-manager/session-manager/start-session?region=ap-south-1) |
| **EC2 → click "Connect"** | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#ConnectToInstance:instanceId=i-0b956d0209231a48b) |
| **CloudShell (AWS CLI in browser, no install)** | [open](https://ap-south-1.console.aws.amazon.com/cloudshell/home?region=ap-south-1) |

### Useful commands after Session Manager opens
```bash
# See app status
sudo systemctl status tickvault

# Live app log
sudo journalctl -u tickvault -f

# Docker containers
docker ps
docker logs tv-questdb --tail 100

# QuestDB query (from box)
curl 'http://localhost:9000/exec?query=SELECT+count(*)+FROM+ticks'

# App's raw Prometheus metrics
curl http://localhost:9091/metrics | head -50

# Disk + memory
df -h
free -h
```

---

# 🗃️ QuestDB Web Console (from your Mac)

The QuestDB web is bound to box's `localhost:9000` for security. Three ways to access:

### Option A — SSM tunnel (one CLI command from your Mac terminal):
```bash
aws ssm start-session \
  --region ap-south-1 \
  --target i-0b956d0209231a48b \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["9000"],"localPortNumber":["9000"]}'
```
Then open **http://localhost:9000** in your browser. Ctrl+C to close.

### Option B — From CloudShell (browser):
1. [Open CloudShell](https://ap-south-1.console.aws.amazon.com/cloudshell/home?region=ap-south-1)
2. Run the same `aws ssm start-session ...` command
3. Forward stops when you close CloudShell tab

### Option C — From Claude Code on your Mac:
```
mcp__tickvault-logs__questdb_sql "SELECT count(*) FROM ticks WHERE ts > dateadd('h', -1, now())"
```

---

# 🚀 DEPLOYS & CI

| What | URL |
|---|---|
| **All GitHub Actions runs** | [open](https://github.com/SJParthi/tickvault/actions) |
| **`deploy-aws` workflow (deploys to prod)** | [open](https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml) |
| **`aws-control` (start/stop/restart-app/stop-app/deploy)** | [open](https://github.com/SJParthi/tickvault/actions/workflows/aws-control.yml) |
| **`terraform-apply` (infra changes)** | [open](https://github.com/SJParthi/tickvault/actions/workflows/terraform-apply.yml) |
| **`aws-autopilot` (auto-heals every 15 min)** | [open](https://github.com/SJParthi/tickvault/actions/workflows/aws-autopilot.yml) |
| **Open PRs** | [open](https://github.com/SJParthi/tickvault/pulls) |
| **All branches** | [open](https://github.com/SJParthi/tickvault/branches) |
| **Manually trigger deploy** | [Run workflow →](https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml) (click "Run workflow" button) |
| **Trigger AWS Control action** | [Run workflow →](https://github.com/SJParthi/tickvault/actions/workflows/aws-control.yml) |

---

# 💰 COSTS & BUDGET

| What | URL |
|---|---|
| **Cost Explorer (last 6 months)** | [open](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/costexplorer) |
| **Bills (monthly statement)** | [open](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/bills) |
| **Budgets (₹2000/mo target)** | [open](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/budgets) |
| **Free Tier usage** | [open](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/freetier) |
| **Payment methods** | [open](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/paymentmethods) |
| **Service quotas** | [open](https://ap-south-1.console.aws.amazon.com/servicequotas/home/services?region=ap-south-1) |

---

# 📡 ALERTS & NOTIFICATIONS

| What | URL |
|---|---|
| **SNS topic `tv-prod-alerts`** (Telegram/Email/SMS) | [open](https://ap-south-1.console.aws.amazon.com/sns/v3/home?region=ap-south-1#/topic/arn:aws:sns:ap-south-1:208384284948:tv-prod-alerts) |
| **SNS topic `tv-prod-budget-kill`** (auto-stop on budget breach) | [open](https://ap-south-1.console.aws.amazon.com/sns/v3/home?region=ap-south-1#/topic/arn:aws:sns:ap-south-1:208384284948:tv-prod-budget-kill) |
| **All SNS topics** | [open](https://ap-south-1.console.aws.amazon.com/sns/v3/home?region=ap-south-1#/topics) |
| **All SNS subscriptions** | [open](https://ap-south-1.console.aws.amazon.com/sns/v3/home?region=ap-south-1#/subscriptions) |
| **Telegram bot chat** | open Telegram app → `Claude Code Task Alerts` |

---

# λ LAMBDA FUNCTIONS

| Function | Console | What it does |
|---|---|---|
| `tv-prod-telegram-webhook` | [open](https://ap-south-1.console.aws.amazon.com/lambda/home?region=ap-south-1#/functions/tv-prod-telegram-webhook) | Posts SNS messages to Telegram |
| `tv-prod-budget-killswitch` | [open](https://ap-south-1.console.aws.amazon.com/lambda/home?region=ap-south-1#/functions/tv-prod-budget-killswitch) | Stops instance at 100% budget |
| `tv-prod-daily-budget-digest` (after PR #934) | [open](https://ap-south-1.console.aws.amazon.com/lambda/home?region=ap-south-1#/functions/tv-prod-daily-budget-digest) | 17:30 IST daily digest |
| `tv-prod-hard-stop-guard` (after PR #934) | [open](https://ap-south-1.console.aws.amazon.com/lambda/home?region=ap-south-1#/functions/tv-prod-hard-stop-guard) | 17:00 IST force stop guard |
| **All Lambdas** | [open](https://ap-south-1.console.aws.amazon.com/lambda/home?region=ap-south-1#/functions) |

---

# 🔐 SECRETS & CONFIG

| What | URL |
|---|---|
| **SSM Parameter Store** (`/tickvault/*`) | [open](https://ap-south-1.console.aws.amazon.com/systems-manager/parameters/?region=ap-south-1) |
| **IAM roles** | [open](https://us-east-1.console.aws.amazon.com/iam/home#/roles) |
| **EC2 instance IAM role** | [open](https://us-east-1.console.aws.amazon.com/iam/home#/roles/details/tv-prod-instance-role) |
| **Security groups** | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#SecurityGroups:) |

---

# 🌐 NETWORK

| What | URL |
|---|---|
| **Elastic IP `tv-prod-eip`** (13.205.212.203) | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#Addresses:) |
| **VPC** | [open](https://ap-south-1.console.aws.amazon.com/vpc/home?region=ap-south-1#vpcs) |
| **Route tables** | [open](https://ap-south-1.console.aws.amazon.com/vpc/home?region=ap-south-1#RouteTables:) |
| **Internet gateways** | [open](https://ap-south-1.console.aws.amazon.com/vpc/home?region=ap-south-1#igws:) |

---

# 💾 STORAGE

| What | URL |
|---|---|
| **EBS volumes** | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#Volumes:) |
| **EBS snapshots** | [open](https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#Snapshots:) |
| **S3 buckets** | [open](https://s3.console.aws.amazon.com/s3/buckets?region=ap-south-1) |
| **S3 `tv-prod-cold`** (deploy binaries) | [open](https://ap-south-1.console.aws.amazon.com/s3/buckets/tv-prod-cold?region=ap-south-1) |

---

# 📅 SCHEDULES

| What | URL |
|---|---|
| **EventBridge rules** (start/stop crons) | [open](https://ap-south-1.console.aws.amazon.com/scheduler/home?region=ap-south-1#schedules) |
| **All scheduled rules** | [open](https://ap-south-1.console.aws.amazon.com/events/home?region=ap-south-1#/rules) |

---

# 🤖 GitHub repository

| What | URL |
|---|---|
| **Repository home** | [open](https://github.com/SJParthi/tickvault) |
| **Issues** | [open](https://github.com/SJParthi/tickvault/issues) |
| **Pull requests** | [open](https://github.com/SJParthi/tickvault/pulls) |
| **Settings → Secrets** | [open](https://github.com/SJParthi/tickvault/settings/secrets/actions) |
| **Settings → Branches** | [open](https://github.com/SJParthi/tickvault/settings/branches) |
| **This page (source)** | [open](https://github.com/SJParthi/tickvault/blob/main/docs/operator/CONSOLE.md) |

---

# 🏦 DHAN (trading account)

| What | URL |
|---|---|
| **Dhan login** | [open](https://login.dhan.co/) |
| **Dhan portal home** | [open](https://web.dhan.co/) |
| **Dhan API help** | mailto:apihelp@dhan.co |
| **Static IP setting** | Dhan portal → Settings → API → Static IP |

---

# 📱 MOBILE APPS (download these on your phone)

| App | Download |
|---|---|
| **AWS Console iOS** | [App Store](https://apps.apple.com/app/aws-console/id607991826) |
| **AWS Console Android** | [Play Store](https://play.google.com/store/apps/details?id=com.amazon.aws.console) |
| **GitHub iOS** | [App Store](https://apps.apple.com/app/github/id1477376905) |
| **GitHub Android** | [Play Store](https://play.google.com/store/apps/details?id=com.github.android) |
| **Telegram** | (already on your phone) |

---

# 🧯 EMERGENCY (When Something's Wrong)

| Symptom | Action |
|---|---|
| Telegram firehose | [AWS Control → action: `stop-app`](https://github.com/SJParthi/tickvault/actions/workflows/aws-control.yml) |
| Want box off entirely | [AWS Control → action: `stop`](https://github.com/SJParthi/tickvault/actions/workflows/aws-control.yml) |
| Want box on | [AWS Control → action: `start`](https://github.com/SJParthi/tickvault/actions/workflows/aws-control.yml) |
| Want to log into box | [Session Manager](https://ap-south-1.console.aws.amazon.com/systems-manager/session-manager/start-session?region=ap-south-1) → pick `i-0b956d0...` |
| Deploy broken | [Latest deploy log](https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml) → click latest red run |
| App crashing | [Live tail logs](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:live-tail) |
| Costs blowing up | [Budgets](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/budgets) → tighten the threshold |
| Lost AWS credentials | [IAM Users](https://us-east-1.console.aws.amazon.com/iam/home#/users) → reset access key |

---

# 🎯 DAILY 30-SECOND ROUTINE

1. **Morning (08:30 IST):** Telegram should show "tickvault started Mode: LIVE" — if not, [check deploys](https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml)
2. **Mid-day:** Glance at [Dashboard](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#dashboards/dashboard/tv-prod-operator) — all green?
3. **Anytime red alarm:** Click [FIRING ALARMS](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarmFilter=ALARM) → see graph → diagnose
4. **17:30 IST:** Telegram shows daily budget digest (after PR #934 merges)
5. **End of week:** [Costs](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/costexplorer) — on track for ₹2000/mo?

---

# 🔑 ACCOUNT-SPECIFIC INFO

```
AWS Account ID:        208384284948
AWS Region:            ap-south-1 (Mumbai)
EC2 Instance ID:       i-0b956d0209231a48b
EC2 Instance Name:     tv-prod-app
EC2 Type:              m8g.large (Graviton ARM)
Elastic IP:            13.205.212.203
EC2 Hostname:          ec2-13-205-212-203.ap-south-1.compute.amazonaws.com
VPC:                   tv-prod-vpc
Public IP DNS:         ec2-13-205-212-203.ap-south-1.compute.amazonaws.com
SNS Alerts Topic:      arn:aws:sns:ap-south-1:208384284948:tv-prod-alerts
SNS Budget Kill:       arn:aws:sns:ap-south-1:208384284948:tv-prod-budget-kill
GitHub repo:           SJParthi/tickvault
GitHub branch:         main
Monthly budget:        ₹2000 INR
Trading window:        09:15-15:30 IST Mon-Fri
EC2 schedule:          08:30 start / 16:30 stop (Mon-Fri) via EventBridge
Hard stop guard:       17:00 IST EVERY day (after PR #934)
Budget digest:         17:30 IST Mon-Fri (after PR #934)
```

---

**Last updated:** 2026-05-31 · **This file:** `docs/operator/CONSOLE.md` · **GitHub URL to bookmark:** https://github.com/SJParthi/tickvault/blob/main/docs/operator/CONSOLE.md
