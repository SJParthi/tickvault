# TickVault — Grafana Cloud operator dashboard (mobile, ₹0, zero box RAM)

One importable dashboard that shows **everything from your phone or laptop** — health
+ live ticks — by URL. It runs in **Grafana's cloud, NOT on your EC2**, so it uses
**no box RAM** and exposes **no public port**.

> Companion guide (account setup, datasource connection, read-only safety): [`docs/operator/mobile-dashboard-setup.md`](../../docs/operator/mobile-dashboard-setup.md).

## What it shows

| Section | Panels | Source |
|---|---|---|
| **Health at a glance** | overall score, market feed, order feed, QuestDB-down, clock skew, token life, silent instruments, ticks dropped, orders rejected | CloudWatch `Tickvault/Prod` (19 real metrics) |
| **Trends** | candles sealed (rising), health score + WS failures over time | CloudWatch |
| **Live ticks** | total ticks, ticks-per-minute, latest 50 ticks | QuestDB (read-only) |

Every panel points at a **real metric the app already publishes** (verified against
the DEPLOYED EMF declaration in `deploy/aws/terraform/user-data.sh.tftpl` — the single
source of truth that `amazon-cloudwatch-agent-ctl -a fetch-config` actually loads on the
box. `deploy/aws/cloudwatch-agent.json` is a byte-for-byte reference copy of that same
declaration, kept in sync by the `cloudwatch_app_alarms_wiring.rs` drift-guard) — nothing invented.

## Import (one-time, ~1 min)

1. In Grafana Cloud: **Dashboards → New → Import**.
2. Upload `tickvault-operator-dashboard.json` (or paste its contents).
3. When prompted, pick your two datasources:
   - **CloudWatch (Tickvault/Prod)** — see mobile-dashboard-setup §3.2 (Assume Role, region `ap-south-1`, read-only).
   - **QuestDB (PostgreSQL via PDC)** — see §3.4 (Private Datasource Connect, host `localhost:8812`, DB `qdb`). *Install the PDC agent only after the box is stable.*
4. **Save** → bookmark the URL on your phone. Auto-refreshes every 30s, timezone IST.

> If the CloudWatch datasource isn't connected yet, the health panels show "No data"
> (not an error) until you finish §3.2. Same for the QuestDB panels and §3.4. The
> dashboard imports fine either way; panels light up as each datasource connects.

## Honest envelope

- This is **view-only observability**. It cannot place orders and does not touch the
  O(1) tick hot path.
- QuestDB read access is **read-only by convention** (panels only `SELECT`). For a
  hard lock, see mobile-dashboard-setup §4 (SELECT-only proxy).
- The 13 health metrics publish at the agent's 60s interval, so health tiles update
  ~once a minute; the QuestDB tick panels are as fresh as the live writes.
