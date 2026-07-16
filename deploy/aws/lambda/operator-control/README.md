# Operator portal — the single-URL mission control

One URL you open on a phone or laptop to run the whole product. Three tabs
(2026-07-02 redesign — operator: "webpage looks completely messy, too many buttons";
2026-07-16 REST-only cleanup — the live-feed-era panels were removed with the live
feeds: Dhan WS retired 2026-07-13, Groww WS retired 2026-07-15; the runtime is
per-minute REST pulls only):

| Tab | What you do |
|---|---|
| 📊 **Overview** | Live status (instance/app/market), the **official-minute-candles hero** (today's rows in `spot_1m_rest` + `option_chain_1m` + `option_contract_1m_rest`, total + per-feed split; '—' when the box is unreachable — never a fabricated 0), the **dedup shield** (the 4 upsert-key columns of `spot_1m_rest`; greyed to one calm "box stopped" banner when the instance is off), **feed toggles** with a `rest_fetch_audit`-sourced per-feed pull line (today's ok / failed / rate-limited + last-hour p50/p99 after minute close), a thin **AWS strip** (spend $ · alarms · disk %, tap to expand), a **latency** card (per-(broker, pull type) p50/p99 of "minute closed → data in hand" from today's fetch log + box-wide QuestDB round-trip, clock skew, and the dormant order-placement shield — it reads "—" until live trading returns), and **ONE context-aware ▶ Start / ■ Stop instance button** |
| 📈 **Data** | Official minute rows captured today per table (spot / chain / contracts) and the **read-only database console** (tables + columns + query grid + CSV download; SELECT/SHOW/EXPLAIN/WITH only, capped 1000 rows) |
| 🛠️ **Admin** | Restart app / Restart QuestDB / Stop app (+ force), a **collapsed danger zone** (severity picker: Wipe ALL data → ☢ Full Docker nuke → Bare Nuke, each with its own typed confirm word; the Wipe-ALL truncate list covers the legacy tables AND the four live REST tables incl. `rest_fetch_audit`), and "Lock / forget this device" |

The former **GitHub** and **Logs** tabs were removed. The `logs` API action is
kept (the tickvault-logs MCP server reads it); the `gh_*` actions were removed
with the tab (deploys/merges happen via GitHub itself). The former ticks/sec
sparkline, tick-conservation shield, per-feed WS probes, cross-verify card and
"Wipe GROWW only" control were removed 2026-07-16 — their producers died with
the live feeds.

## Security model

- `GET /` serves the page — a **public, inert shell with zero secrets**.
- Every `POST` action requires `Authorization: Bearer <secret>` (constant-time compare). The **token is the device key**: saved only in that device's browser localStorage. Only your laptop + phone hold it → in practice only those devices work. "🔒 Lock / forget this device" wipes it.
- IAM scoped to: this one instance (ec2 start/stop/reboot, ssm send+read), the control-secret SSM param, and read-only `cloudwatch:DescribeAlarms` + `ce:GetCostAndUsage`. Nothing else. (The former GitHub-PAT read went with the GitHub tab; Terraform may still set `OPERATOR_GITHUB_TOKEN_PARAM` — the Lambda ignores it. IAM/terraform cleanup is a follow-up.)
- The SQL box is **read-only** — mutating keywords are rejected before the query reaches QuestDB.
- Destructive box actions are blocked 09:15–15:30 IST Mon–Fri unless you tick **force**.
- **DATA-destructive actions (Wipe ALL / Docker reset / Bare Nuke) are HARD-LOCKED 09:15–15:30 IST Mon–Fri — refused even with force.** A mid-market wipe destroys data that can never be re-fetched (2026-07-02 incident: a forced 15:05 IST wipe deleted ~4.5M rows + 77s of live feed). The danger zone shows a 🔒 lock label while the market is open; run these after 15:30.

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

150 pure-function tests: method routing, constant-time bearer auth, market-hours guard, the REST-era snapshot parsers (hero counts, per-feed splits, rest_fetch_audit pull stats + latency percentiles), the read-only SQL gate, wipe/nuke guards + confirm tokens (incl. the extended Wipe-ALL verification over the four live REST tables), the 3-tab structure, honest-degrade pins (no fabricated zeros), and the legacy-panel resurrection ratchets. The boto3 / SSM action paths are exercised by the live deploy smoke test.
