# 📱 TickVault — Mobile + Laptop Dashboard (one beautiful URL, anywhere)

> **Goal:** see **everything** — live ticks, feed health, alarms — from your **phone or
> laptop**, by URL, with attractive real-time visuals. **No terminal. No SSM. No AWS console.**
>
> **Cost:** ₹0 (free tiers). **RAM on your trading box:** ~0 (the dashboard runs in
> Grafana's cloud, NOT on your EC2). **Public ports exposed:** none (read-only,
> outbound-only connector).
>
> **Honest envelope:** This is *observability* (viewing). It does not change the
> O(1) tick hot-path (already O(1)) and cannot place orders. QuestDB read access is
> "read-only by convention" via SELECT-only panels — see §4 for the hard-lock option.

---

## 0. The picture (what you end up with)

```
  YOUR PHONE / LAPTOP                 GRAFANA CLOUD (free, hosted)            YOUR AWS (ap-south-1)
  ┌────────────────┐   one URL   ┌────────────────────────────┐  read-only ┌─────────────────────┐
  │  Grafana app   │ ──────────► │  • Health dashboard         │ ─────────► │ CloudWatch metrics  │
  │  or browser    │             │    (CloudWatch metrics)     │            │ (feed/app/alarms)   │
  │  bookmark      │             │  • Live tick charts +       │ ─private─► │ QuestDB (ticks)     │
  └────────────────┘             │    ad-hoc SELECT (QuestDB)  │  connector └─────────────────────┘
                                 └────────────────────────────┘   (no public port, no DROP risk)
```

---

## 1. Comparison — why Grafana Cloud is the pick

| What you want | Grafana Cloud (⭐ pick) | Custom S3+CloudFront | CloudWatch public URL | AWS Console |
|---|---|---|---|---|
| Beautiful / animated | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐ | ⭐ |
| Works on phone (app + URL) | ✅ official app | ✅ URL | ✅ URL | ⚠️ clunky |
| RAM used on your trading box | **0** (Grafana-hosted) | 0 | 0 | 0 |
| Cost | **₹0** free tier | ~₹50–100/mo | ₹0 | ₹0 |
| Shows CloudWatch health | ✅ | ✅ (build) | ✅ | ✅ |
| Shows **live QuestDB ticks** | ✅ via private connector | ⚠️ needs API | ❌ | ❌ |
| Public port exposed | **none** | none | none | none |
| Can it `DROP`/write your data? | ❌ no (SELECT panels) | ❌ | ❌ | ⚠️ console can |

---

## 2. Read-only QuestDB from mobile — the safe options

| Approach | Hard-enforced read-only? | Box RAM | Public port? | Notes |
|---|---|---|---|---|
| **⭐ Grafana Cloud + Private Datasource Connect (PDC)** | "by convention" (panels run SELECT only) | tiny agent | **none — outbound only** | recommended; nothing reachable from internet |
| SELECT-only API proxy (we build) | ✅ **enforced** (rejects non-SELECT) | small | 1 authed port | add if you want it literally impossible to write |
| Expose QuestDB `:9000` | ❌ **anyone can wipe data** | 0 | yes | **NEVER do this** |
| SSH/SSM tunnel (today) | ✅ | 0 | no | laptop-only, manual |

**Recommendation:** Grafana Cloud + PDC. If you want the *hard* read-only lock, say so and
we add the SELECT-only proxy as a second layer.

---

## 3. Setup — Grafana Cloud (free), ~10 minutes, one-time

### 3.1 Create the free account
1. Go to **grafana.com → "Create free account"** → pick a stack name (e.g. `tickvault`).
2. You now have a permanent URL like `https://tickvault.grafana.net` — **bookmark it on your phone.**
3. Install the **Grafana** app from the App Store / Play Store, sign in → same dashboards on mobile.

### 3.2 Connect CloudWatch (health metrics) — read-only
1. In Grafana: **Connections → Add new connection → CloudWatch**.
2. Auth = **AWS SDK / Assume Role**. Grafana shows you **its AWS account ID + an External ID** — copy both.
3. In your AWS account, create a **read-only role** that trusts Grafana with that External ID and
   attaches the policy below (CloudWatch + Logs read-only — **no write, no DB, no EC2 control**):

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "GrafanaCloudWatchReadOnly",
      "Effect": "Allow",
      "Action": [
        "cloudwatch:DescribeAlarmsForMetric",
        "cloudwatch:DescribeAlarmHistory",
        "cloudwatch:DescribeAlarms",
        "cloudwatch:ListMetrics",
        "cloudwatch:GetMetricData",
        "cloudwatch:GetMetricStatistics",
        "cloudwatch:GetInsightRuleReport",
        "logs:DescribeLogGroups",
        "logs:GetLogGroupFields",
        "logs:StartQuery",
        "logs:StopQuery",
        "logs:GetQueryResults",
        "logs:GetLogEvents",
        "ec2:DescribeRegions",
        "tag:GetResources"
      ],
      "Resource": "*"
    }
  ]
}
```
   Trust policy (replace `<GRAFANA_AWS_ACCOUNT_ID>` and `<EXTERNAL_ID>` with the values Grafana showed):
```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": { "AWS": "arn:aws:iam::<GRAFANA_AWS_ACCOUNT_ID>:root" },
    "Action": "sts:AssumeRole",
    "Condition": { "StringEquals": { "sts:ExternalId": "<EXTERNAL_ID>" } }
  }]
}
```
4. Paste the role ARN back into Grafana, region = `ap-south-1`. **Test → Save.**

### 3.3 Health dashboard — metrics that already flow to CloudWatch
Namespace `Tickvault/Prod`, dimension `host`. Add panels for:

| Panel | Metric | Healthy = |
|---|---|---|
| Feed alive | `tv_websocket_pool_all_dead` | `0` |
| Order feed alive | `tv_order_update_ws_active` | `≥ 1` |
| QuestDB connected | `tv_questdb_disconnected_seconds` | `0` |
| Silent instruments | `tv_tick_gap_instruments_silent` | `0` |
| Token life left | `tv_token_remaining_seconds` | `> 14400` |
| Ticks dropped | `tv_spill_dropped_total` *(retired 2026-07-18 — stage-4 dead-producer sweep; zero emit sites)* | `0` |
| Candles sealed | `tv_aggregator_seals_emitted_total` | rising |
| Overall score | `tv_realtime_guarantee_score` | `≥ 0.95` |
| Clock skew | `tv_clock_skew_seconds` | `< 2` |

### 3.4 Live ticks (QuestDB) — read-only, no public port
1. In Grafana: **Connections → Private data source connect (PDC)** → follow the wizard. It gives a
   one-line installer for a **tiny outbound-only agent** that runs on the EC2 box. **It opens NO
   inbound port** — Grafana reaches QuestDB *through* the agent's outbound tunnel.
   > ⚠️ Install this agent **only after the box is stable** (post-cooldown) so it doesn't disturb recovery.
2. Add a **PostgreSQL datasource** → host `localhost:8812` (via PDC) → DB `qdb` → creds from
   SSM `/tickvault/staging/questdb/*`.
3. Panels / Explore — example read-only queries (these only ever SELECT):
```sql
SELECT count(*) FROM ticks;                                  -- total ticks today
SELECT * FROM ticks ORDER BY exchange_timestamp DESC LIMIT 50;  -- latest 50
SELECT security_id, count(*) FROM ticks GROUP BY security_id;   -- per-instrument
```

---

## 4. Want it HARD read-only (literally impossible to write)?
Open-source QuestDB has no enforced read-only DB role, so §3.4 is "read-only by convention"
(Grafana panels only SELECT). For a guaranteed lock, we add a **SELECT-only API proxy**: a small
authed endpoint that parses the query and rejects anything that isn't a `SELECT`. Ask and we build it.

---

## 5. What's already automatic (no clicks, no commands)
- **08:30 IST** EventBridge auto-starts the box; **16:30** auto-stops it.
- App auto-boots, auto-reconnects, auto-recovers.
- Auto-deploy on merge to `main`.
- Telegram alerts on any failure (already on your phone).

## 6. One-tap control (no terminal) — already exists
**GitHub mobile app → Actions → "AWS Control" → Run workflow → pick:**
`status` · `start` · `stop` · `restart-app` · `stop-app` · `restart-questdb` · `rotate-ip` · `deploy`.

### `rotate-ip` — clear a Dhan IP-level feed penalty in one tap
If the feed shows **"Connection reset / 0 of 1 connected"** but the *same* token
works on another machine (e.g. your laptop), Dhan has rate-penalised the box's
**IP**, not your account. One tap on `rotate-ip` swaps the instance's public
Elastic IP for a fresh one, releases the old (so it isn't billed), and restarts
the app so the feed dials out from the clean IP — then watch Telegram for
**"1 of 1 feeds connected"**.

| | Manual (AWS console) | `rotate-ip` button |
|---|---|---|
| Steps | allocate → associate → release → restart (4) | **1 tap** |
| Time | ~2 min | ~30 s |
| Mistake risk | medium | ~zero (guards built in) |

> Only valid in the **no-orders data-pull phase** — Dhan's static-IP *Order*
> whitelist does **not** gate the market-data feed, so swapping the IP is safe.
> Do it **once** per penalty; repeatedly hopping IPs can escalate Dhan to an
> account-level block.

---

## 7. Your phone bookmarks (the whole product in 3 links)
1. **Grafana** `https://<your-stack>.grafana.net` — health + live ticks (this guide)
2. **GitHub Actions** — start/stop/deploy buttons
3. **Telegram** — alerts

That's the entire system, mobile-first, ₹0, zero box RAM, nothing exposed to the internet.
