# AWS Budget Enforcement — ₹5,000/mo Hard Cap

> **Authority:** Parthiban (architect). Non-negotiable.
> **Scope:** Any file touching AWS deployment, infrastructure, Docker config, or cost-impacting changes.

## Budget: ₹5,000/month MAXIMUM (all AWS services included)

### Approved AWS Bill (verified from AWS Pricing API, ap-south-1, 2026-04-08)

| Component | Spec | Unit Price (USD) | Monthly ₹ |
|-----------|------|-----------------|-----------|
| EC2 (weekdays) | c7i.xlarge, on-demand, 9hr × 22 weekdays | $0.1785/hr | ₹3,530 |
| EC2 (weekends) | c7i.xlarge, on-demand, 5hr × 8 weekends | $0.1785/hr | ₹607 |
| EBS | gp3, 100GB (hot data: last 30-60 days) | $0.0912/GB-mo | ₹775 |
| S3 | Up to 500GB cold data (lifecycle tiered) | $0.025→$0.002/GB-mo | ₹333 |
| Elastic IP | 1 static IP (mandatory for Dhan Order API) | $0.005/hr idle | ₹152 |
| CloudWatch | ALWAYS ON: 7 metrics + 5 alarms + 2GB logs | FREE (within free tier) | ₹0 |
| SNS | ~100 SMS alerts/month (India) | $0.00278/msg | ₹25 |
| Data Transfer | ~10GB outbound | ~$0.01/GB | ₹85 |
| **TOTAL** | | | **₹4,981** |
| **Buffer** | | | **₹19** |

> Worst case with 500GB S3 cold data. Actual S3 cost starts at ₹106 (50GB)
> and grows ~₹10/mo. Takes 4-5 years to reach 500GB. Budget holds.

### Pricing Type: ON-DEMAND (not reserved, not spot)
- No upfront commitment. Pay only when instance is running.
- Weekdays: 8:30 AM – 5:30 PM IST (9hr, auto via EventBridge Scheduler).
- Weekends: 8:30 AM – 1:30 PM IST (5hr, auto via EventBridge Scheduler).
- Manual: start/stop anytime via AWS Console or CLI for extra checks.
- Cost per hour: **₹15.17** ($0.1785 × ₹85).

## Data Lifecycle: Hot (EBS) → Cold (S3) — Up to 500GB

| Data Age | Storage Tier | Size | ₹/GB-mo | ₹/mo |
|----------|-------------|------|---------|------|
| 0-60 days | EBS gp3 (100GB) | ~100GB | ₹7.75 | ₹775 |
| 60-365 days | S3 Intelligent-Tiering | ~150GB | ₹1.17 | ₹176 |
| 1-5 years | S3 Glacier Deep Archive | ~300GB | ₹0.17 | ₹51 |
| **Total (worst case)** | | **500GB** | | **₹333/mo on S3** |

S3 lifecycle policy (auto, no code needed):
- 0-90 days after upload: S3 Standard ($0.025/GB)
- 90-365 days: S3 Intelligent-Tiering ($0.0138/GB) — auto
- >365 days: Glacier Deep Archive ($0.002/GB) — auto
- SEBI 5-year retention: satisfied at ₹0.17/GB/mo

Partition manager (already implemented) detaches QuestDB partitions older than retention_days.
S3 archival exports detached partitions before removal (Plan Item 7, needs aws-sdk-s3).

## Mechanical Rules

1. **NEVER provision a larger instance than c7i.xlarge** without Parthiban's approval.
2. **NEVER increase EBS beyond 100GB** without Parthiban's approval.
   If 100GB fills up, partition manager detaches old data → export to S3 → free space.
3. **NEVER add paid AWS services** (RDS, ElastiCache, NAT Gateway, etc.) without budget review.
   AWS ALB allowed within free tier (750 hrs/mo) replacing Traefik.
4. **CloudWatch is MANDATORY and always enabled** — monitoring, logging, alerting are non-negotiable.
   Free tier covers: 10 custom metrics, 10 alarms, 5GB logs, 3 dashboards. Stay within free tier.
5. **S3 lifecycle policy is MANDATORY** — auto-tier to Intelligent-Tiering (90d) → Glacier (365d).
   Keeps 500GB cold data under ₹333/mo instead of ₹1,063/mo.
6. **Docker memory budget for c7i.xlarge / c8g.xlarge (8GB)** — Wave 7-A trimmed (2026-05-10):
   - QuestDB: **2GB** (down from 4GB; cascading mat views removed in Wave 6)
   - Valkey: **512MB** (down from 1GB; token + instrument cache fits easily)
   - Prometheus: **384MB** (down from 512MB; 7d retention)
   - Grafana: **768MB** (down from 1GB)
   - **Alertmanager: 256MB (KEPT — independent process, app-crash alert safety)**
   - Tickvault app: ~500MB
   - Total Docker: ~4.0GB. Remaining: ~4.0GB for OS + buffer.
   - **REMOVED in Wave 7-A:** Traefik (use AWS ALB free tier), valkey-exporter (not queried).
   - **Already removed:** Jaeger, Loki, Alloy (saves 2.5GB RAM).
7. **Manual starts budgeted at 20hr/month max.** If consistently exceeding, review schedule.
8. **HTTP gateway:** Use AWS ALB (free tier 750 hrs/mo) for HTTPS termination, or app on port 3001 directly behind security group for internal-only access.
9. **Alert routing:** Prometheus → Alertmanager → Telegram (standard pattern). Alertmanager runs as an independent container so app-crash alerts still reach the operator. Routing alerts through the app itself is a single-point-of-failure anti-pattern (rejected 2026-05-10).
10. **NO manual configuration on AWS deployment** — every setting (memory, schedule, alerts, dashboards, audits) lives in version-controlled config files. `git clone` + `docker compose up -d` reproduces full stack identically on Mac dev and AWS prod.

## What This Prevents

- Accidentally provisioning c7i.2xlarge (doubles EC2 cost to ₹6,008)
- EBS growing unbounded (200GB = ₹1,550, 500GB = ₹3,876 — blows budget)
- Adding CloudWatch Logs beyond free tier (₹0.67/GB adds up fast)
- Forgetting Elastic IP charges when instance is stopped
- Adding managed services that balloon the bill
- S3 costs exploding (Intelligent-Tiering + Glacier keep cold data cheap)
- App-crash alerts vanishing (Alertmanager independence prevents this)
- Manual config drift between Mac and AWS (single compose file)

## Instance Schedule (Wave 7-A — 2026-05-10 update)

- **Weekday Start:** 8:30 AM IST Mon-Fri (EventBridge: `cron(0 3 ? * MON-FRI *)`)
- **Weekday Stop:** 5:30 PM IST Mon-Fri (EventBridge: `cron(0 12 ? * MON-FRI *)`)
- **Weekend Start:** 8:30 AM IST Sat-Sun (EventBridge: `cron(0 3 ? * SAT,SUN *)`)
- **Weekend Stop:** 1:30 PM IST Sat-Sun (EventBridge: `cron(0 8 ? * SAT,SUN *)`)
- **Manual Start:** Anytime — `aws ec2 start-instances --instance-ids i-xxx` or AWS Console
- **Manual Stop:** Anytime — `aws ec2 stop-instances --instance-ids i-xxx` or AWS Console
- **Static IP:** Elastic IP stays associated 24/7 (never release — Dhan static IP has 7-day cooldown)
- **Skip weekends to save:** Each skipped weekend day saves ₹76 (5hr × ₹15.17)

> **Note:** Schedule shifted from 7:45→17:15 to 8:30→17:30 (still 9 hours, ~₹170/mo savings). Aligns with Wave 6 plan post-market 1m fetch window: market closes 15:30, fetch + cross-verify + persist by 17:25, AWS auto-stops 17:30 with 5 min buffer.

## Cost Per Hour Reference

| Action | Cost |
|--------|------|
| Instance running | ₹15.17/hr ($0.1785) |
| Instance stopped (EBS + EIP) | ₹1.43/hr ($0.0168) |
| 1 weekend day (8hr manual) | ₹121 |
| 1 quick check (2hr manual) | ₹30 |

## RAM Trim Audit (Wave 7-A, 2026-05-10 — Alertmanager retained)

| Service | Pre-Wave-7-A | Post-Wave-7-A | Saved |
|---|---|---|---|
| QuestDB | 4 GB | 2 GB | -2 GB |
| Valkey | 1 GB | 512 MB | -512 MB |
| Grafana | 1 GB | 768 MB | -256 MB |
| Prometheus | 512 MB | 384 MB | -128 MB |
| **Alertmanager** | 256 MB | **256 MB (KEPT)** | 0 |
| Traefik | 512 MB | REMOVED | -512 MB |
| Valkey-exporter | 128 MB | REMOVED | -128 MB |
| **Total Docker** | **~7.4 GB** | **~4.0 GB** | **-3.4 GB** |
| **Headroom on 8 GB** | ~600 MB | **~4.0 GB** | massive |

## 100% Coverage Verification (after Wave 7-A trim)

| Need | Tool | Status |
|---|---|---|
| Tracking | QuestDB audit tables (15+) | ✅ KEEP |
| Logging | tracing → errors.jsonl + CloudWatch Logs | ✅ KEEP local + add CW |
| Monitoring | Prometheus (14 new metrics in Wave 6 plan) | ✅ KEEP |
| Alerting | Prometheus → Alertmanager → Telegram (standard pattern) | ✅ KEEP (independent process) |
| Auditing | QuestDB audit tables + S3 cold archive | ✅ KEEP |
| Capturing | QuestDB ticks + candles | ✅ KEEP |
| Visualizing | Grafana | ✅ KEEP |
| Dashboards | Grafana operator-health single page | ✅ KEEP |
| HTTP gateway | AWS ALB (free tier) | ✅ replaces Traefik |
| Distributed tracing | CloudWatch X-Ray (optional, free tier 100K traces/mo) | ⚠️ optional |

## Common Runtime / Dynamic / Scalable / Automated Charter (mandatory)

Operator demand 2026-05-10: "extremely common runtime dynamic scalable approach,
fully comprehensively automated, logged, tracked, captured, visualized, alerted,
notified on Telegram, no manual inputs."

| Demand | Mechanical enforcement |
|---|---|
| Common runtime | Same `docker-compose.yml` Mac dev = AWS prod (rule 10) |
| Dynamic | EventBridge auto-start/stop, dynamic depth-20/200 selectors, dynamic Phase 2 dispatch, dynamic SLO score 10s |
| Scalable | Bounded mpsc + spill ring + DLQ; horizontal: depth-20 5×50, depth-200 5×1; QuestDB partition manager prunes old data automatically |
| Automated logging | tracing macros mandatory; ERROR auto-routes to Telegram via Alertmanager; hourly errors.jsonl rotation; 48h retention sweep auto |
| Automated tracking | 15+ audit tables auto-INSERT on every typed event with DEDUP UPSERT KEYS |
| Automated capturing | every tick auto-persists to QuestDB; spill NDJSON catches overflow; auto-replays on rehydration |
| Automated visualizing | Grafana dashboards auto-provisioned via `grafana/provisioning/`; operator-health single-page renders without setup |
| Automated alerting | Prometheus alert rules in `alerts.yml` evaluate every 30s; Alertmanager auto-routes by severity to Telegram |
| Automated notifications | teloxide Telegram client + Alertmanager webhook; `Severity::High`/`Critical` auto-page operator |
| No manual inputs | Boot sequence is fully automatic: bootstrap.sh pulls SSM → Docker compose up → app self-tests via `make doctor` → 3-tier fallback (cache → SSM → TOTP) → market-open self-test at 09:16:30 IST |

## Trigger

This rule activates when editing files matching:
- `deploy/docker/docker-compose.yml`
- `deploy/aws/*`
- `scripts/aws-*`
- `crates/app/src/infra.rs`
- Any file containing `c7i`, `c8g`, `mem_limit`, `EBS`, `gp3`, `instance_type`, `aws_region`
