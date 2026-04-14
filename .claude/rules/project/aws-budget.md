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
- Weekdays: 8AM-5PM IST (9hr, auto via EventBridge Scheduler).
- Weekends: 8AM-1PM IST (5hr, auto via EventBridge Scheduler).
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
3. **NEVER add paid AWS services** (RDS, ElastiCache, ALB, NAT Gateway, etc.) without budget review.
4. **CloudWatch is MANDATORY and always enabled** — monitoring, logging, alerting are non-negotiable.
   Free tier covers: 10 custom metrics, 10 alarms, 5GB logs, 3 dashboards. Stay within free tier.
5. **S3 lifecycle policy is MANDATORY** — auto-tier to Intelligent-Tiering (90d) → Glacier (365d).
   Keeps 500GB cold data under ₹333/mo instead of ₹1,063/mo.
6. **Docker memory budget for c7i.xlarge (8GB)**:
   - QuestDB: 4GB | Valkey: 1GB | Prometheus: 512MB | Grafana: 1GB
   - Alertmanager: 256MB | Traefik: 512MB | Valkey-exporter: 128MB
   - Total: 7.4GB. Remaining: 600MB for OS.
   - Jaeger, Loki, Alloy: REMOVED (saves 2.5GB RAM).
7. **Manual starts budgeted at 20hr/month max.** If consistently exceeding, review schedule.

## What This Prevents

- Accidentally provisioning c7i.2xlarge (doubles EC2 cost to ₹6,008)
- EBS growing unbounded (200GB = ₹1,550, 500GB = ₹3,876 — blows budget)
- Adding CloudWatch Logs beyond free tier (₹0.67/GB adds up fast)
- Forgetting Elastic IP charges when instance is stopped
- Adding managed services that balloon the bill
- S3 costs exploding (Intelligent-Tiering + Glacier keep cold data cheap)

## Instance Schedule

- **Weekday Start:** 7:45 AM IST Mon-Fri (EventBridge: `cron(15 2 ? * MON-FRI *)`)
- **Weekday Stop:** 5:15 PM IST Mon-Fri (EventBridge: `cron(45 11 ? * MON-FRI *)`)
- **Weekend Start:** 7:45 AM IST Sat-Sun (EventBridge: `cron(15 2 ? * SAT,SUN *)`)
- **Weekend Stop:** 1:15 PM IST Sat-Sun (EventBridge: `cron(45 7 ? * SAT,SUN *)`)
- **Manual Start:** Anytime — `aws ec2 start-instances --instance-ids i-xxx` or AWS Console
- **Manual Stop:** Anytime — `aws ec2 stop-instances --instance-ids i-xxx` or AWS Console
- **Static IP:** Elastic IP stays associated 24/7 (never release — Dhan static IP has 7-day cooldown)
- **Skip weekends to save:** Each skipped weekend day saves ₹76 (5hr × ₹15.17)

## Cost Per Hour Reference

| Action | Cost |
|--------|------|
| Instance running | ₹15.17/hr ($0.1785) |
| Instance stopped (EBS + EIP) | ₹1.43/hr ($0.0168) |
| 1 weekend day (8hr manual) | ₹121 |
| 1 quick check (2hr manual) | ₹30 |

## Trigger

This rule activates when editing files matching:
- `deploy/docker/docker-compose.yml`
- `deploy/aws/*`
- `scripts/aws-*`
- `crates/app/src/infra.rs`
- Any file containing `c7i`, `mem_limit`, `EBS`, `gp3`, `instance_type`, `aws_region`
