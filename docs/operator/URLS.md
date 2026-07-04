# Tickvault — Operator URLs (Bookmark This Page)

> **One page = every URL you need to see what's happening in production.**
> Save this page in your browser. Click any link → see live data.

---

## ⚡ Top 5 (Open These First Every Morning)

| # | What | URL |
|---|---|---|
| 1 | **Live App Dashboard** (all metrics, real-time) | [CloudWatch Dashboard `tv-prod-operator`](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#dashboards/dashboard/tv-prod-operator) |
| 2 | **Live App Logs** (stream every line) | [Log group `/tickvault/prod/app` → click "Start tailing"](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Ftickvault$252Fprod$252Fapp) |
| 3 | **Active Alarms** (red = action needed) | [CloudWatch Alarms](https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarmFilter=ALARM) |
| 4 | **Deploy History** | [GitHub Actions → `deploy-aws`](https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml) |
| 5 | **Cost / Bill** | [Cost Explorer](https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/costexplorer) |

---

## 🟢 Real-Time Health (Refreshes Every 60 Seconds)

| What | URL |
|---|---|
| Dashboard `tv-prod-operator` | https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#dashboards/dashboard/tv-prod-operator |
| ALL CloudWatch Alarms | https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2: |
| Firing Alarms ONLY | https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#alarmsV2:alarmFilter=ALARM |

## 📜 Live Logs (Stream Every Line)

| What | URL |
|---|---|
| App logs `/tickvault/prod/app` | https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Ftickvault$252Fprod$252Fapp |
| Telegram webhook Lambda | https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Faws$252Flambda$252Ftv-prod-telegram-webhook |
| Budget kill-switch Lambda | https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:log-groups/log-group/$252Faws$252Flambda$252Ftv-prod-budget-killswitch |
| Live Tail (real-time stream) | https://ap-south-1.console.aws.amazon.com/cloudwatch/home?region=ap-south-1#logsV2:live-tail |

## 🖥️ Box Access (Like SSH but Browser-Based)

| What | URL |
|---|---|
| Session Manager (browser terminal) | https://ap-south-1.console.aws.amazon.com/systems-manager/session-manager/start-session?region=ap-south-1 |
| EC2 instance `tv-prod-app` | https://ap-south-1.console.aws.amazon.com/ec2/home?region=ap-south-1#InstanceDetails:instanceId=i-0b956d0209231a48b |
| CloudShell (AWS CLI in browser) | https://ap-south-1.console.aws.amazon.com/cloudshell/home?region=ap-south-1 |

### Useful commands after Session Manager opens

```bash
docker ps                                    # see QuestDB container
docker logs tv-questdb --tail 50             # QuestDB logs
sudo journalctl -u tickvault -f              # app live log
curl http://localhost:9000                   # QuestDB web (curl-only from box)
curl http://localhost:9091/metrics           # app's raw Prometheus metrics
```

## 🌐 QuestDB Web Console (from MacBook)

QuestDB is bound to `localhost:9000` on the box for security. Two ways to view from your laptop:

**Option A — Quick SSM tunnel (run from your Mac terminal):**

```bash
aws ssm start-session \
  --region ap-south-1 \
  --target i-0b956d0209231a48b \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["9000"],"localPortNumber":["9000"]}'
```

Then open **http://localhost:9000** in Chrome. Press Ctrl+C in terminal to close.

**Option B — Tailscale Funnel:** RETIRED for QuestDB (2026-07-04 security
hardening). The funnel is public internet with no auth, so the raw QuestDB
SQL port (9000) is no longer funneled — use Option A (SSM port-forward)
above. The funnel now fronts ONLY the tickvault API on port 3001, and its
`/api/debug/*` routes require the bearer token.

## 🚀 Deploys & Code

| What | URL |
|---|---|
| GitHub repo | https://github.com/SJParthi/tickvault |
| All workflow runs | https://github.com/SJParthi/tickvault/actions |
| `deploy-aws` workflow runs | https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml |
| Open PRs | https://github.com/SJParthi/tickvault/pulls |
| Trigger deploy manually | https://github.com/SJParthi/tickvault/actions/workflows/deploy-aws.yml → "Run workflow" |
| AWS Control workflow (start/stop/restart/stop-app) | https://github.com/SJParthi/tickvault/actions/workflows/aws-control.yml |

## 💰 Cost & Budget

| What | URL |
|---|---|
| Cost Explorer (last 6 months) | https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/costexplorer |
| Budgets (₹2000/mo target) | https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/budgets |
| Bills (monthly statement) | https://ap-south-1.console.aws.amazon.com/billing/home?region=ap-south-1#!/bills |

## 📡 Alerting & Notifications

| What | URL |
|---|---|
| SNS topic `tv-prod-alerts` (Telegram/Email/SMS fan-out) | https://ap-south-1.console.aws.amazon.com/sns/v3/home?region=ap-south-1#/topic/arn:aws:sns:ap-south-1:208384284948:tv-prod-alerts |
| SNS topic `tv-prod-budget-kill` (kills box if bill blows) | https://ap-south-1.console.aws.amazon.com/sns/v3/home?region=ap-south-1#/topic/arn:aws:sns:ap-south-1:208384284948:tv-prod-budget-kill |
| Lambda `tv-prod-telegram-webhook` | https://ap-south-1.console.aws.amazon.com/lambda/home?region=ap-south-1#/functions/tv-prod-telegram-webhook |
| Lambda `tv-prod-budget-killswitch` | https://ap-south-1.console.aws.amazon.com/lambda/home?region=ap-south-1#/functions/tv-prod-budget-killswitch |
| Telegram bot chat | (open Telegram app → `Claude Code Task Alerts`) |

## 📅 Schedules

| What | URL |
|---|---|
| EventBridge rules (Mon-Fri 08:30/16:30 IST start/stop) | https://ap-south-1.console.aws.amazon.com/events/home?region=ap-south-1#/rules |

## 🏦 Trading (Dhan)

| What | URL |
|---|---|
| Dhan account login | https://login.dhan.co/ |
| Dhan API support | mailto:apihelp@dhan.co |
| Dhan static IP setting | https://login.dhan.co/ → My Account → API Settings |

---

## 📱 Mobile

- **AWS Console Mobile App:**
  - [iOS App Store](https://apps.apple.com/app/aws-console/id607991826)
  - [Google Play](https://play.google.com/store/apps/details?id=com.amazon.aws.console)
- **GitHub Mobile:**
  - [iOS](https://apps.apple.com/app/github/id1477376905)
  - [Android](https://play.google.com/store/apps/details?id=com.github.android)
- **Telegram:** already on your phone

---

## 🎯 Daily 30-Second Health Check

1. Open **Dashboard** (URL #1 above) — all widgets green? ✅
2. Glance at **Alarms** (URL #3) — any red? 🔴 → click → diagnose
3. Open **Telegram** → scroll recent messages — anything 🆘 or ⚠️?
4. Done. Coffee.

---

## 🧯 Emergency / "Something's Wrong"

| Symptom | Action |
|---|---|
| Telegram firehose | GitHub Actions → AWS Control → action: `stop-app` → Run workflow |
| Want box off entirely | GitHub Actions → AWS Control → action: `stop` → Run workflow |
| Want to log into the box | Session Manager URL above → Start Session → pick `i-0b956d0...` |
| Lost track of deploys | GitHub Actions → deploy-aws (URL above) → latest run |
| Costs blowing up | Cost Explorer (above) → AWS Budgets → set tighter limit |
| App crashing repeatedly | CloudWatch Logs `/tickvault/prod/app` → look for `ERROR` |

---

**Last updated:** 2026-05-31
**Auto-regenerated by deploy:** no (this is a static page — update manually if URLs change)
