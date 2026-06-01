# Operator-control Lambda — the single-URL console

One URL you open on a phone or laptop to **see** the trading box (instance up?
app up? ticks flowing? candles sealing? recent errors?) **and control** it
(start / stop / restart-app / restart-questdb). No AWS console, no GitHub UI,
no Grafana.

## How it works

| Request | Auth | What it does |
|---|---|---|
| `GET /` (open the URL in a browser) | **none** | Serves the console page — a static shell with **zero secrets**. It shows a token box, a status panel, and control buttons. |
| `POST /` `{ "action": "view" }` | **Bearer token** | Live snapshot: instance state, app up/down, today's tick count, per-timeframe candle counts, last 5 errors, **plus live guarantee proof** — the deployed `ticks` dedup-key column count (4 = the sub-second fix is live) and the max ticks-per-second today (>1 = sub-second capture working). |
| `POST /` `{ "action": "start\|stop\|reboot\|restart-app\|stop-app\|restart-questdb" }` | **Bearer token** | Runs the scoped action on the one instance. |

The token you type is stored **only in your browser** (localStorage) and sent
as `Authorization: Bearer <token>` on every POST. It never appears in the page
source, Terraform state, or the Lambda env.

Destructive actions (`stop`, `reboot`, `restart-app`, `stop-app`) are blocked
09:15–15:30 IST Mon–Fri unless you tick **force**.

`deploy` is intentionally NOT here — deploy auto-fires when a PR merges to
`main` touching `crates/**`. This console links you to the box, not GitHub.

## One-time enable (feature-flagged OFF by default — zero cost/risk until you opt in)

```bash
# 1. Create the bearer secret (pick a long random string).
aws ssm put-parameter --region ap-south-1 \
  --name /tickvault/prod/operator/control-secret \
  --type SecureString \
  --value "$(openssl rand -base64 30)"

# 2. Turn on the feature flag and apply (creates the Lambda + Function URL + IAM).
cd deploy/aws/terraform
terraform apply -var enable_operator_control_lambda=true

# 3. Grab the URL (open it in a browser; paste the secret from step 1 once).
terraform output operator_control_function_url
```

To read the secret back later: `aws ssm get-parameter --name
/tickvault/prod/operator/control-secret --with-decryption --query
Parameter.Value --output text`.

## Security model

- Function URL AWS auth = `NONE`, but every POST is gated by a constant-time
  bearer compare against the SSM SecureString. Missing/wrong → `401`.
- IAM scoped to **exactly** this instance: `ec2:Start/Stop/Reboot` on the one
  instance ARN, `ssm:SendCommand` + `ssm:GetCommandInvocation` on the one
  instance + `AWS-RunShellScript` only, `ssm:GetParameter` on the one secret.
  Nothing else.
- Every invocation is CloudWatch-logged; a `tv-prod-operator-control-errors`
  alarm pages if the Lambda starts erroring.

## Tests

```bash
cd deploy/aws/lambda/operator-control
python3 -m unittest test_handler -v
```

15 pure-function tests (method routing, bearer auth, market-hours guard,
snapshot parsing, public-HTML-has-no-secret). The boto3 EC2/SSM action paths
are exercised by the live deploy smoke test.
