# Access tickvault on AWS from anywhere — securely

> **What this gives you:** open the **QuestDB web console** and the **app API**
> in your own browser (Mac / IntelliJ / phone-with-laptop) while they run on
> the AWS box — **without exposing anything to the public internet**. The
> tunnel is authenticated by your AWS login (IAM) and encrypted end-to-end via
> AWS Systems Manager (SSM). No SSH key, no public IP, no open ports.
>
> **Why not a plain public URL?** The QuestDB web console has **no password** —
> a public URL would let anyone who finds the IP read all your market data and
> run queries, right next to your broker credentials. SSM port-forwarding gives
> the identical "open it in my browser" experience with zero exposure. This is
> the recommended, secure way and works from anywhere with the AWS CLI.

---

## One-time prerequisites (on whatever laptop you use)

```bash
brew install awscli                 # AWS CLI v2
# the Session Manager plugin (needed for port-forwarding):
#   https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html
aws configure                       # region = ap-south-1, your IAM keys
aws sts get-caller-identity         # must print your account
```

Instance id: **`i-0b956d0209231a48b`** (`tv-prod-app`). Region: `ap-south-1`.

---

## 1. The CloudWatch dashboard — no tunnel needed

The visual operator page (app health, market-data flow, alarms) is in the AWS
Console — open it from **any browser, even your phone**, just by logging into
AWS:

> AWS Console → CloudWatch → Dashboards → **`tv-prod-operator`**

Or get the direct URL after deploy:
```bash
cd deploy/aws/terraform && terraform output dashboard_url
```

This needs **no tunnel** — it's a native AWS console page. Logs, metrics,
alarms, and the live alarm-status strip are all there.

---

## 2. QuestDB web console (browse/query your market data)

QuestDB's console runs on the box at `127.0.0.1:9000` (localhost-only = secure).

**One command (recommended):**

```bash
make questdb-prod        # or: bash scripts/questdb-tunnel.sh [--port N] [--no-open]
```

This resolves the running `tv-prod-app` instance dynamically (no hardcoded
instance id), preflights the AWS CLI / credentials / session-manager-plugin
with actionable errors, opens the SSM tunnel, waits until the local port is
live, auto-opens the browser, and holds the tunnel until you press `Ctrl-C`.
If local port 9000 is busy (e.g. local QuestDB running), it falls back to
19000 and tells you. If the box is stopped, it says so and exits (or pass
`--wait-for-box 30` to poll until it comes up).

**Auto-open every trading morning (macOS, one-time install):**

```bash
make questdb-autoopen-install     # launchd agent: weekdays 08:35 IST,
                                  # waits up to 30 min for the box
                                  # (it auto-starts 08:30 IST Mon-Fri)
make questdb-autoopen-uninstall   # remove it
```

Logs land in `~/Library/Logs/tickvault-questdb-tunnel.log`. The agent needs
`aws` credentials + `session-manager-plugin` available on the Mac (same
preflights as the manual run — failures are logged there, not silent).

**Manual equivalent** (what the script does under the hood):

```bash
aws ssm start-session \
  --region ap-south-1 \
  --target i-0b956d0209231a48b \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["9000"],"localPortNumber":["9000"]}'
```

Leave that running, then open **http://localhost:9000** in your browser — it's
the live QuestDB console on the AWS box. Run SQL, browse `ticks`,
`candles_*`, `option_chain_minute_snapshot`, the instrument lifecycle, etc.

Press `Ctrl+C` in the terminal to close the tunnel when done.

---

## 3. App REST API (health, stats, quotes)

The app API runs on **port 3002 in staging** (3001 in prod). Tunnel it:

```bash
aws ssm start-session \
  --region ap-south-1 \
  --target i-0b956d0209231a48b \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["3002"],"localPortNumber":["3002"]}'
```

Then on your laptop (the API needs the bearer token from
`/tickvault/staging/api/bearer-token`):
```bash
TOKEN=$(aws ssm get-parameter --region ap-south-1 \
  --name /tickvault/staging/api/bearer-token --with-decryption \
  --query Parameter.Value --output text)

curl -s -H "Authorization: Bearer $TOKEN" http://localhost:3002/health
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:3002/api/stats
```

> Note: the app API DOES have bearer-token auth, so unlike the QuestDB console
> it *could* be safely exposed publicly later if you ever want a raw URL — but
> the tunnel is simpler and needs no extra infra.

---

## 4. A shell on the box (logs, systemctl, make doctor)

No SSH key needed — Session Manager:
```bash
aws ssm start-session --region ap-south-1 --target i-0b956d0209231a48b
#   inside the box:
#     systemctl status tickvault --no-pager
#     journalctl -u tickvault -n 100 --no-pager
#     cd /opt/tickvault && cat data/logs/errors.summary.md
```

Or the repo's Makefile shortcuts (from `~/tickvault`):
```bash
make aws-ssm            # interactive shell
make aws-ssm-command    # tails the last 50 app-log lines
make aws-outputs        # instance_id, dashboard_url, etc.
make aws-cost           # current month bill estimate
```

---

## 5. Logs — three ways, all from AWS

1. **CloudWatch Logs** (from any browser): Console → CloudWatch → Log groups →
   `/tickvault/prod/app` and `/tickvault/prod/system`.
2. **On the box**: `journalctl -u tickvault -f` (live) via `make aws-ssm`.
3. **Structured ERROR summary**: `data/logs/errors.summary.md` on the box.

---

## Security posture (what's locked down)

| Layer | State |
|---|---|
| Secrets | Encrypted in SSM Parameter Store (SecureString); never in repo/logs |
| QuestDB console (:9000) | localhost-only on the box; reachable ONLY via your SSM tunnel |
| App API (:3002) | localhost-only + bearer-token auth |
| SSH (:22) | locked to your operator IP CIDR only |
| Public internet | nothing exposed without auth; no open DB/API ports |
| Instance metadata | IMDSv2 required |
| Access audit | every SSM session is logged in CloudTrail (who/when/what) |

**The "from anywhere" you wanted = AWS Console (dashboard/logs/alarms) + SSM
tunnels (QuestDB/API).** Works from Mac, IntelliJ terminal, or any laptop with
the AWS CLI — fully independent of your local Macbook, fully secure.

---

## Want a true public URL later?

Two safe options (NOT the raw QuestDB console — it has no auth):
- **App API behind the bearer token** on the Elastic IP (when EIP is enabled
  for live trading) — already authenticated.
- A small auth proxy in front of QuestDB — a future enhancement if you want
  browser access without the CLI tunnel.

Ask and I'll wire either — but the SSM tunnel above already gives you secure
browser access from anywhere today.
