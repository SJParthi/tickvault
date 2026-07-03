# Operator portal — the single-URL mission control

One URL you open on a phone or laptop to run the whole product. Three tabs
(2026-07-02 redesign — operator: "webpage looks completely messy, too many buttons"):

| Tab | What you do |
|---|---|
| 📊 **Overview** | Live status (instance/app/market), count-up tick counter, ticks/sec sparkline, **guarantee shields** incl. the audit-backed **Tick conservation** verdict (15:40 IST `tick_conservation_audit` + today's `ws_event_audit` disconnects — never green from a tick rate alone; greyed to one calm "box stopped" banner when the instance is off), feed toggles, a thin **AWS strip** (spend $ · alarms · disk %, tap to expand), a compact **latency** card ("Measure now"), and **ONE context-aware ▶ Start / ■ Stop instance button** |
| 📈 **Data** | Animated candle bars per timeframe, the daily **cross-verify** result, and the **read-only database console** (tables + columns + query grid + CSV download; SELECT/SHOW/EXPLAIN/WITH only, capped 1000 rows) |
| 🛠️ **Admin** | Restart app / Restart QuestDB / Stop app (+ force), a **collapsed danger zone** (severity picker: Wipe GROWW only → Wipe ALL data → Full Docker reset → Bare Nuke, each with its own typed confirm word), and "Lock / forget this device" |

The former **GitHub** and **Logs** tabs were removed. The `logs` API action is
kept (the tickvault-logs MCP server reads it); the `gh_*` actions were removed
with the tab (deploys/merges happen via GitHub itself).

## Security model

- `GET /` serves the page — a **public, inert shell with zero secrets**.
- Every `POST` action requires `Authorization: Bearer <secret>` (constant-time compare). The **token is the device key**: saved only in that device's browser localStorage. Only your laptop + phone hold it → in practice only those devices work. "🔒 Lock / forget this device" wipes it.
- IAM scoped to: this one instance (ec2 start/stop/reboot, ssm send+read), the control-secret SSM param, and read-only `cloudwatch:DescribeAlarms` + `ce:GetCostAndUsage`. Nothing else. (The former GitHub-PAT read went with the GitHub tab; Terraform may still set `OPERATOR_GITHUB_TOKEN_PARAM` — the Lambda ignores it. IAM/terraform cleanup is a follow-up.)
- The SQL box is **read-only** — mutating keywords are rejected before the query reaches QuestDB.
- Destructive box actions are blocked 09:15–15:30 IST Mon–Fri unless you tick **force**.
- **DATA-destructive actions (Wipe GROWW / Wipe ALL / Docker reset / Bare Nuke) are HARD-LOCKED 09:15–15:30 IST Mon–Fri — refused even with force.** A mid-market wipe destroys data that can never be re-fetched (2026-07-02 incident: a forced 15:05 IST wipe deleted ~4.5M rows + 77s of live feed). The danger zone shows a 🔒 lock label while the market is open; run these after 15:30.

## Zero-touch enable (prod, via CI)

Prod opts in through `terraform-apply.yml` (`TF_VAR_enable_operator_control_lambda=true`).
On the next apply the workflow **creates the Lambda + Function URL, generates your
device key in SSM (once), and DMs you one tappable link on Telegram with the key
baked into the URL fragment** (`…/#key=…`). Open it → you're unlocked, no typing.
The key is never printed to the Actions log; the fragment never reaches the server.
**Do not forward that Telegram message** — the link is your key.

## Manual enable (local — the terraform variable default stays OFF)

```bash
# 1. Console bearer key (your device key — paste it into the page once)
aws ssm put-parameter --region ap-south-1 \
  --name /tickvault/prod/operator/control-secret \
  --type SecureString --value "$(openssl rand -base64 30)"

# 2. Turn the portal on (creates Lambda + Function URL + scoped IAM)
cd deploy/aws/terraform
terraform apply -var enable_operator_control_lambda=true

# 3. Get your URL — open it, paste the device key once
terraform output operator_control_function_url
```

## Tests

```bash
cd deploy/aws/lambda/operator-control
python3 -m unittest test_handler -v
```

109 pure-function tests: method routing, constant-time bearer auth, market-hours guard, snapshot parsing, the read-only SQL gate, wipe/nuke guards + confirm tokens, the 3-tab structure (danger zone collapsed, severity-picker mapping, context-aware start/stop, stopped-box banner), and public-HTML-has-no-secret. The boto3 / SSM action paths are exercised by the live deploy smoke test.
