# Operator portal — the single-URL mission control

One URL you open on a phone or laptop to run the whole product. Tabs:

| Tab | What you do |
|---|---|
| 📊 **Overview** | Live status (instance/app/market), count-up tick counter, ticks/sec sparkline, **live no-loss proof** (dedup key cols + peak ticks/sec), and Start / Stop / Restart-app / Restart-QuestDB buttons |
| 📈 **Data** | Animated candle bars per timeframe + a **read-only QuestDB query box** (SELECT/SHOW/EXPLAIN/WITH only, capped 200 rows) |
| 🔀 **GitHub** | See open PRs + their CI status, **squash-merge** a PR, and **trigger a deploy** — all from the page |
| 📜 **Logs** | Live error tail + app log tail from the box (journald) |
| ☁️ **AWS** | CloudWatch alarms firing + this month's AWS spend |
| ⚡ **Latency** | Real measured per-tick latency (network RTT to Dhan, TLS, QuestDB round-trip, clock skew, per-tick process ns + wire→done ns from the app metrics). See `docs/architecture/aws-vs-home-latency.md`. |

## Security model

- `GET /` serves the page — a **public, inert shell with zero secrets**.
- Every `POST` action requires `Authorization: Bearer <secret>` (constant-time compare). The **token is the device key**: saved only in that device's browser localStorage. Only your laptop + phone hold it → in practice only those devices work. "🔒 Lock / forget this device" wipes it.
- GitHub actions use a **fine-grained PAT scoped to the one repo**, read from SSM at runtime — never in the page, env, or Terraform state.
- IAM scoped to: this one instance (ec2 start/stop/reboot, ssm send+read), the two SSM secrets, and read-only `cloudwatch:DescribeAlarms` + `ce:GetCostAndUsage`. Nothing else.
- The SQL box is **read-only** — mutating keywords are rejected before the query reaches QuestDB.
- Destructive box actions are blocked 09:15–15:30 IST Mon–Fri unless you tick **force**.

## One-time enable (feature-flagged OFF by default — zero cost/risk until you opt in)

```bash
# 1. Console bearer key (your device key — paste it into the page once)
aws ssm put-parameter --region ap-south-1 \
  --name /tickvault/prod/operator/control-secret \
  --type SecureString --value "$(openssl rand -base64 30)"

# 2. Fine-grained GitHub PAT — scope it to ONLY SJParthi/tickvault with:
#      Contents: read, Pull requests: read+write, Actions: read+write, Checks: read
#    (create at https://github.com/settings/personal-access-tokens)
aws ssm put-parameter --region ap-south-1 \
  --name /tickvault/prod/operator/github-token \
  --type SecureString --value "github_pat_xxx"

# 3. Turn the portal on (creates Lambda + Function URL + scoped IAM)
cd deploy/aws/terraform
terraform apply -var enable_operator_control_lambda=true

# 4. Get your URL — open it, paste the device key once
terraform output operator_control_function_url
```

The GitHub tab simply shows "github token not configured" until step 2 is done; everything else works without it.

## Tests

```bash
cd deploy/aws/lambda/operator-control
python3 -m unittest test_handler -v
```

19 pure-function tests: method routing, constant-time bearer auth, market-hours guard, snapshot parsing, the read-only SQL gate, all-tabs-present, and public-HTML-has-no-secret. The boto3 / GitHub / SSM action paths are exercised by the live deploy smoke test.
